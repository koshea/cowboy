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
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};

use super::transport::channel::{self, EngineChannels};
use super::transport::{EgressTransport, NftTransport, TransportConfig};

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
    /// Returns the session and the **policy engine's ends** of the relay channels.
    /// The caller must serve them; until it does, every connection and every lookup
    /// the sandbox attempts blocks waiting for a verdict — the right failure
    /// direction.
    pub fn start(name: &str, holder_exe: &Path) -> Result<(Self, EngineChannels)> {
        let (engine_connect, relay_connect) =
            channel::pair().context("creating the relay connect channel")?;
        let (engine_resolve, relay_resolve) =
            channel::pair().context("creating the relay resolve channel")?;
        let mut cmd = Command::new("unshare");
        cmd.args(["--user", "--map-root-user", "--net", "--ipc", "--uts", "--"]);
        cmd.arg(holder_exe).arg("x-sandbox-holder");
        // stdin is the liveness channel: the holder blocks reading it, so if this
        // process dies the read returns EOF and the holder exits, taking the
        // namespaces with it. No orphaned session namespaces after a crash.
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Hand the relay its ends of the channels on fixed descriptors. Rust marks
        // descriptors CLOEXEC, so they must be duplicated onto the agreed numbers in
        // the child — and dup2 clears CLOEXEC, which is what lets them survive the
        // exec.
        let sources = [
            (relay_connect.as_raw_fd(), channel::CONNECT_FD),
            (relay_resolve.as_raw_fd(), channel::RESOLVE_FD),
        ];
        // SAFETY: `pre_exec` runs between fork and exec; fcntl, dup2 and close are
        // async-signal-safe and touch only this child's descriptor table.
        unsafe {
            cmd.pre_exec(move || {
                // Move both sources clear of the target numbers first. A source may
                // *already* be 3 or 4 — the numbers the kernel hands out are not ours
                // to choose — and a naive dup2 would then clobber the other channel.
                let mut staged = [0; 2];
                for (i, (src, _)) in sources.iter().enumerate() {
                    let high = libc::fcntl(*src, libc::F_DUPFD_CLOEXEC, 16);
                    if high == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    staged[i] = high;
                }
                for (i, (_, target)) in sources.iter().enumerate() {
                    if libc::dup2(staged[i], *target) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(staged[i]);
                }
                Ok(())
            });
        }

        let mut holder = cmd.spawn().context(
            "starting the sandbox session holder (is util-linux `unshare` installed, and are \
             unprivileged user namespaces enabled?)",
        )?;
        // Our copies are not needed once the child holds them, and keeping them open
        // would stop the brokers ever seeing end-of-stream when the relay exits.
        drop(relay_connect);
        drop(relay_resolve);
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

        Ok((
            Self {
                holder,
                pid,
                name: name.to_string(),
            },
            EngineChannels {
                connect: engine_connect,
                resolve: engine_resolve,
            },
        ))
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

/// `cowboy x-sandbox-holder` — sets up the session's network and serves as its relay.
///
/// Runs *inside* the new namespaces (started via `unshare`), where it holds
/// `CAP_NET_ADMIN` in its own user namespace and can therefore do the one-time setup
/// no agent process is ever able to: bring loopback up, create the black-hole device,
/// and load the interception ruleset.
///
/// Readiness is signalled **after** all of that and after the relay is listening, so
/// there is no window in which a command could run before enforcement exists. Then it
/// blocks on stdin, so closing it — or the worker dying — ends the session.
///
/// It is also the relay. That keeps the topology small and gets the isolation for
/// free: the holder does not unshare a PID namespace, while every agent command runs
/// in its own, so no agent process can see the holder at all — which is what protects
/// the channels it holds to the policy engine.
///
/// Two listeners, both dumb pipes: TCP for intercepted connections, UDP for DNS.
/// Neither holds any policy, and the DNS one does not even hold the address of a
/// resolver — it forwards query bytes to the host and returns whatever comes back.
pub async fn run_holder() -> Result<()> {
    {
        // Loopback is DOWN in a fresh network namespace, so without this every
        // connection to 127.0.0.1 inside the sandbox fails — including the agent's
        // to the relay.
        bring_loopback_up().await?;

        // The relay channels arrive on known descriptors. Their absence means the
        // worker did not set up a policy path, so there is nothing to enforce with:
        // refuse rather than come up with unpoliced networking. Both are required —
        // coming up with connections policed but DNS unpoliced would leave the query
        // itself as an open exfiltration channel.
        let connect_channel = take_channel_fd(channel::CONNECT_FD).context(
            "the relay connect channel was not passed to the holder; refusing to \
             bring up a session with no policy path",
        )?;
        let resolve_channel = take_channel_fd(channel::RESOLVE_FD).context(
            "the relay resolve channel was not passed to the holder; refusing to \
             bring up a session whose DNS would be unpoliced",
        )?;

        let cfg = TransportConfig::default();
        let transport = NftTransport::new(cfg.clone());
        transport
            .install(&cfg)
            .await
            .context("installing egress interception")?;
        transport
            .verify()
            .await
            .context("verifying egress interception")?;

        // Listen before signalling readiness: a command that started first would
        // find the redirect in place but nothing accepting on the other end.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", cfg.relay_port))
            .await
            .with_context(|| format!("binding the relay on 127.0.0.1:{}", cfg.relay_port))?;
        let resolver = tokio::net::UdpSocket::bind(("127.0.0.1", cfg.dns_port))
            .await
            .with_context(|| format!("binding the resolver on 127.0.0.1:{}", cfg.dns_port))?;
        tokio::spawn(async move {
            if let Err(e) = super::transport::relay::serve(listener, connect_channel).await {
                tracing::error!(error = %e, "relay exited");
            }
        });
        tokio::spawn(async move {
            if let Err(e) = super::transport::relay::serve_dns(resolver, resolve_channel).await {
                tracing::error!(error = %e, "dns relay exited");
            }
        });

        println!("{READY}");
        use std::io::Write;
        std::io::stdout().flush().ok();

        // Block until stdin closes. Reading (rather than sleeping forever) is what
        // ties the session's lifetime to its parent's.
        tokio::task::spawn_blocking(|| {
            let mut buf = [0u8; 1];
            loop {
                match std::io::stdin().read(&mut buf) {
                    Ok(0) => return, // parent gone
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                }
            }
        })
        .await
        .ok();
        Ok(())
    }
}

async fn bring_loopback_up() -> Result<()> {
    let out = tokio::process::Command::new("ip")
        .args(["link", "set", "lo", "up"])
        .output()
        .await
        .context("running `ip link set lo up` (is iproute2 installed?)")?;
    if !out.status.success() {
        bail!(
            "could not bring loopback up in the session network namespace: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Adopt a relay channel from its inherited descriptor.
///
/// Returns `None` when nothing was passed, which the caller treats as fatal.
fn take_channel_fd(fd: std::os::fd::RawFd) -> Option<OwnedFd> {
    use std::os::fd::FromRawFd;
    // Confirm something is actually there before claiming ownership: adopting a
    // closed descriptor would fail later and much less clearly.
    // SAFETY: fcntl with F_GETFD only queries the descriptor.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return None;
    }
    // SAFETY: the worker passed this descriptor and does not use it in the child.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
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
