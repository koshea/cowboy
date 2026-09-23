//! Running one command inside the sandbox.
//!
//! The command is a **local child process** rather than a request to a daemon, so
//! its lifecycle is ours to manage directly. That is a real simplification over the
//! Docker path, which had to record a pgid in a file inside the container and sweep
//! `/proc` by env marker to catch descendants that re-`setsid`ed out of the
//! recorded group.
//!
//! Here, `--unshare-pid` plus `--die-with-parent` means killing bwrap takes down
//! PID 1 of the sandbox namespace and the kernel reaps everything in it —
//! including a process that deliberately escaped its process group. Verified; see
//! `docs/src/security/sandbox-decisions.md`.

use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use cowboy_sandbox::SandboxPlan;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::bwrap::{self, NetMode};
use super::session::SessionSandbox;
use super::shim::ShimRequest;
use super::stream::LineSplitter;
use super::ExecResult;

/// Exit code reported for a command stopped by its timeout, matching the
/// shell convention the Docker path used.
pub(crate) const EXIT_TIMEOUT: i32 = 124;
/// Exit code reported for a command the user interrupted.
pub(crate) const EXIT_CANCELLED: i32 = 130;
/// How long a command gets to exit after `SIGTERM` before `SIGKILL`.
const GRACE: Duration = Duration::from_secs(2);

/// One command to run in the sandbox.
pub struct ExecRequest<'a> {
    pub plan: &'a SandboxPlan,
    pub command: &'a str,
    /// Working directory inside the sandbox; defaults to the plan's workdir.
    pub cwd: Option<&'a str>,
    /// Wall-clock bound, 0 for unbounded.
    pub timeout_secs: u64,
    pub net: NetMode,
    /// Session whose namespaces this command joins. `None` runs standalone, which
    /// only makes sense together with [`NetMode::Isolated`].
    pub session: Option<&'a SessionSandbox>,
}

/// Build the bwrap command for a plan, entering `session` if given.
///
/// Shared by every spawn path so they cannot drift in how they confine things —
/// a streaming command, a background process and an interactive shell must all get
/// the same boundary.
fn build_command(
    plan: &SandboxPlan,
    shell_command: &str,
    net: NetMode,
    session: Option<&SessionSandbox>,
) -> Result<(Command, ShimRequest)> {
    let bwrap_path = bwrap::resolve_bwrap()?;
    // The shim runs from inside the sandbox at a fixed path; the plan binds the
    // cowboy binary there (see `cowboy_sandbox::SHIM_PATH`).
    //
    // Checked rather than assumed, because the failure is otherwise unreadable. Binds
    // are rendered `--ro-bind-try` (an optional path that vanished between planning and
    // now should not abort the command), so a shim whose source is missing is *silently
    // skipped* — and bwrap then reports `execvp /.cowboy-shim: No such file or
    // directory`, which points at the target inside the sandbox and says nothing about
    // the host path that actually went away. Every command in the session fails that
    // way, so the agent stays alive with no ability to run anything.
    //
    // The way this happens in practice is upgrading cowboy (`cargo install`) while a
    // session is running: `current_exe()` then reads `".../cowboy (deleted)"`.
    // `project::self_exe` resolves that, so this is the backstop, not the fix.
    ensure_shim_is_bound(plan)?;
    // An overlay whose write layers cannot be created — or a kernel with no
    // overlayfs — is dropped, not fatal. It is an optimisation (reuse the host's
    // toolchains instead of downloading our own); losing it costs time, whereas
    // letting bwrap fail the mount would break every command in the session.
    let usable = usable_overlays(plan);
    let filtered;
    let plan = if usable.len() == plan.overlays.len() {
        plan
    } else {
        filtered = SandboxPlan {
            overlays: usable,
            ..plan.clone()
        };
        &filtered
    };
    let shim_argv: Vec<OsString> = vec![cowboy_sandbox::SHIM_PATH.into(), "x-sandbox-shim".into()];
    let argv = bwrap::build_argv(&bwrap_path, plan, net, &shim_argv);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(session) = session {
        // tokio's Command wraps a std one; pre_exec is installed on the inner value.
        session.enter(cmd.as_std_mut())?;
    }
    // Its own process group, so a signal aimed at the command cannot reach cowboy.
    cmd.process_group(0);

    let request = ShimRequest {
        command: shell_command.to_string(),
        read_only: to_strings(&plan.landlock.read_only),
        read_write: to_strings(&plan.landlock.read_write),
        list_dirs: to_strings(&plan.landlock.list_dirs),
        scope_ipc: plan.landlock.scope_ipc,
        deny_syscalls: plan.seccomp.denied.iter().map(|s| s.to_string()).collect(),
        deny_raw_sockets: plan.seccomp.deny_raw_sockets,
    };
    Ok((cmd, request))
}

/// Whether this kernel offers overlayfs at all.
///
/// Cheap and cached: the answer cannot change while the machine is up, and this is
/// on the path of every command.
fn overlayfs_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        std::fs::read_to_string("/proc/filesystems")
            .map(|s| {
                s.lines()
                    .any(|l| l.split_whitespace().last() == Some("overlay"))
            })
            .unwrap_or(false)
    })
}

/// The overlays that can actually be mounted, creating their write layers.
///
/// Anything that cannot be prepared is left out rather than raised: see the call
/// site. `upper` and `work` must be on the same filesystem, which they are — both
/// live under the project root.
fn usable_overlays(plan: &SandboxPlan) -> Vec<cowboy_sandbox::plan::Overlay> {
    if plan.overlays.is_empty() || !overlayfs_available() {
        return Vec::new();
    }
    plan.overlays
        .iter()
        .filter(|o| {
            let made = std::fs::create_dir_all(&o.upper)
                .and_then(|_| std::fs::create_dir_all(&o.work))
                .and_then(|_| ignore_marker(o));
            if let Err(e) = &made {
                tracing::debug!(
                    upper = %o.upper.display(),
                    error = %e,
                    "overlay write layer could not be created; falling back to a private store"
                );
            }
            made.is_ok() && o.lower.exists()
        })
        .cloned()
        .collect()
}

/// Keep the overlay's write layers out of `git status`.
///
/// A `.gitignore` of `*` in the parent of `upper`/`work`, so a store that grows to
/// gigabytes never shows up as untracked — and without needing the project's own
/// `.gitignore` to have been updated, which existing projects would not have.
///
/// On the parent rather than inside `upper`: a file in `upper` appears in the
/// merged view, i.e. as litter inside the user's toolchain store.
fn ignore_marker(o: &cowboy_sandbox::plan::Overlay) -> std::io::Result<()> {
    let Some(parent) = o.upper.parent() else {
        return Ok(());
    };
    let marker = parent.join(".gitignore");
    if marker.exists() {
        return Ok(());
    }
    std::fs::write(marker, "*\n")
}

/// Refuse to run when the lockdown shim would not be present inside the sandbox.
///
/// Fails closed either way — without the shim nothing execs — so this exists purely to
/// replace an error that names the wrong thing with one that names the right thing and
/// says what to do.
fn ensure_shim_is_bound(plan: &SandboxPlan) -> Result<()> {
    let bound = plan
        .binds
        .iter()
        .find(|b| b.target == cowboy_sandbox::SHIM_PATH);
    match bound {
        Some(b) if b.source.exists() => Ok(()),
        Some(b) => anyhow::bail!(
            "the cowboy binary is no longer at {}, so the sandbox lockdown shim cannot be \
             mounted. This usually means cowboy was upgraded or moved while this session was \
             running. End the session and start a new one (`cowboy down`, then re-run).",
            b.source.display()
        ),
        None => anyhow::bail!(
            "the sandbox plan has no lockdown shim, so no command can be confined; refusing to \
             run. Check `cowboy doctor` and that the cowboy binary is on disk."
        ),
    }
}

/// Send the shim its request, then any payload for the command itself.
///
/// The request is one **newline-terminated** compact JSON object. The shim reads up
/// to that newline and no further, so whatever follows is inherited as the command's
/// own stdin — which is how the structured file tools pass multi-line content
/// without it ever having to survive shell quoting.
async fn send_request(
    child: &mut tokio::process::Child,
    request: &ShimRequest,
    payload: Option<&str>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().context("sandbox stdin unavailable")?;
    let mut buf = serde_json::to_vec(request)?;
    debug_assert!(!buf.contains(&b'\n'), "the request must be a single line");
    buf.push(b'\n');
    if let Some(p) = payload {
        buf.extend_from_slice(p.as_bytes());
    }
    stdin
        .write_all(&buf)
        .await
        .context("sending the shim request")?;
    // Closing signals EOF to the command reading the payload.
    stdin.shutdown().await.ok();
    Ok(())
}

/// Run a command, streaming combined stdout and stderr to `chunks` as it arrives.
///
/// Returns the exit status and the full accumulated output. Interrupts via
/// `cancel` or the timeout kill the whole sandbox, not just the leader.
pub async fn run_streaming(
    req: ExecRequest<'_>,
    cancel: CancellationToken,
    chunks: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<(ExecResult, String)> {
    let shell_command = shell_command_for(&req);
    let (mut cmd, request) = build_command(req.plan, &shell_command, req.net, req.session)?;
    // Combined output on one pipe keeps interleaving faithful to what the command
    // actually produced; two pipes would reorder it. No PTY: over a plain pipe,
    // tools like cargo and mise emit plain streamable lines instead of
    // cursor-movement progress we would have to emulate.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Reap on drop as a backstop; the explicit kill path below is the normal one.
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context("spawning the sandbox")?;
    // Record what this pid is running, so a network approval can say which command wants
    // the destination instead of only quoting a pid. Held for the life of the call: the
    // guard's `Drop` removes the entry on every exit path, including a timeout or a
    // cancellation, so a later command cannot inherit this label after pid reuse.
    //
    // SECURITY: the string is the command the *host* passed to bwrap, taken before the
    // command can run. It is display-only — see `sandbox::attribution`.
    let _attributed = child
        .id()
        .map(|pid| crate::sandbox::attribution::record(pid, &shell_command));
    send_request(&mut child, &request, None).await?;

    let mut stdout = child.stdout.take().context("sandbox stdout unavailable")?;
    let mut stderr = child.stderr.take().context("sandbox stderr unavailable")?;

    let mut accumulated = String::new();
    let mut splitter = LineSplitter::new();
    let mut buf_out = vec![0u8; 8192];
    let mut buf_err = vec![0u8; 8192];
    let mut out_done = false;
    let mut err_done = false;

    let timeout = if req.timeout_secs == 0 {
        Duration::from_secs(86_400)
    } else {
        Duration::from_secs(req.timeout_secs)
    };
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut interrupted: Option<&str> = None;
    while !(out_done && err_done) {
        tokio::select! {
            r = stdout.read(&mut buf_out), if !out_done => match r {
                Ok(0) | Err(_) => out_done = true,
                Ok(n) => splitter.feed(&buf_out[..n], &mut accumulated, &chunks),
            },
            r = stderr.read(&mut buf_err), if !err_done => match r {
                Ok(0) | Err(_) => err_done = true,
                Ok(n) => splitter.feed(&buf_err[..n], &mut accumulated, &chunks),
            },
            _ = cancel.cancelled() => { interrupted = Some("cancelled"); break; }
            _ = &mut deadline => { interrupted = Some("timed out"); break; }
        }
    }
    splitter.finish(&mut accumulated, &chunks);

    if let Some(why) = interrupted {
        terminate(&mut child).await;
        let note = format!("[command {why}]");
        accumulated.push_str(&note);
        let _ = chunks.send(format!("{note}\n"));
        return Ok((
            ExecResult {
                exit_code: if why == "timed out" {
                    EXIT_TIMEOUT
                } else {
                    EXIT_CANCELLED
                },
            },
            accumulated,
        ));
    }

    let status = child.wait().await.context("waiting for the sandbox")?;
    Ok((
        ExecResult {
            exit_code: status.code().unwrap_or(-1),
        },
        accumulated,
    ))
}

/// Stop the sandbox: `SIGTERM` for a chance to clean up, then `SIGKILL`.
///
/// Signalling bwrap is sufficient because `--die-with-parent` propagates its death
/// to PID 1 of the sandbox namespace, after which the kernel kills every remaining
/// process there. That covers descendants which re-`setsid`ed and so would have
/// survived a process-group signal.
async fn terminate(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    // SAFETY: `kill` with a valid pid; a race where the child already exited is
    // reported as ESRCH and ignored.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    match tokio::time::timeout(GRACE, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.start_kill();
            // Reap so the process does not linger as a zombie.
            let _ = tokio::time::timeout(GRACE, child.wait()).await;
        }
    }
}

/// The shell command, prefixed with a `cd` when a working directory is requested.
///
/// Quoted so a directory containing spaces or shell metacharacters cannot break
/// out into command position, and `&&` so a missing directory fails the command
/// instead of silently running it somewhere else.
fn shell_command_for(req: &ExecRequest<'_>) -> String {
    match req.cwd {
        Some(cwd) if cwd != req.plan.workdir => {
            format!("cd {} && {}", shell_quote(cwd), req.command)
        }
        _ => req.command.to_string(),
    }
}

/// Single-quote a string for POSIX `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Spawn a long-running process and return without waiting for it.
///
/// Used for the background processes declared in `agent.yaml`. It shares the
/// session's network namespace so later commands can reach it on loopback, but has
/// its own PID namespace, so killing the returned child reaps exactly its own
/// process tree.
///
/// Its Landlock domain is fixed here and can never be widened, so a grant approved
/// later is invisible to it — see `NativeSandbox::warn_about_stale_processes`.
pub async fn spawn_detached(
    plan: &SandboxPlan,
    command: &str,
    cwd: Option<&str>,
    net: NetMode,
    session: Option<&SessionSandbox>,
) -> Result<tokio::process::Child> {
    let shell_command = match cwd {
        Some(cwd) if cwd != plan.workdir => format!("cd {} && {command}", shell_quote(cwd)),
        _ => command.to_string(),
    };
    let (mut cmd, request) = build_command(plan, &shell_command, net, session)?;
    cmd.stdin(Stdio::piped());
    // Output goes nowhere: a background process's logs belong in its own file under
    // the workspace, not interleaved into the agent's transcript.
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // Must NOT be reaped when the handle is dropped — outliving the command that
    // started it is the entire point of a background process.
    cmd.kill_on_drop(false);

    let mut child = cmd.spawn().context("spawning the background process")?;
    send_request(&mut child, &request, None).await?;
    Ok(child)
}

/// Run a command with `payload` on its stdin, capturing output.
///
/// The structured file tools use this so multi-line content never has to survive
/// shell quoting. The payload follows the shim's request line on the same pipe; see
/// [`send_request`].
pub async fn run_with_stdin(
    plan: &SandboxPlan,
    command: &str,
    payload: &str,
    net: NetMode,
    session: Option<&SessionSandbox>,
) -> Result<(ExecResult, String)> {
    let (mut cmd, request) = build_command(plan, command, net, session)?;
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context("spawning the sandbox")?;
    // The file tools reach the network far less often than a shell command, but "which
    // command wants this?" should not answer "unknown" just because the caller was `write`.
    let _attributed = child
        .id()
        .map(|pid| crate::sandbox::attribution::record(pid, command));
    send_request(&mut child, &request, Some(payload)).await?;

    let out = child
        .wait_with_output()
        .await
        .context("waiting for the sandbox")?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((
        ExecResult {
            exit_code: out.status.code().unwrap_or(-1),
        },
        combined,
    ))
}

/// Run an interactive command, inheriting the terminal.
pub async fn run_interactive(
    plan: &SandboxPlan,
    command: &str,
    net: NetMode,
    session: Option<&SessionSandbox>,
) -> Result<ExecResult> {
    use tokio::io::AsyncWriteExt;
    let (mut cmd, request) = build_command(plan, command, net, session)?;
    cmd.stdin(Stdio::piped());
    // stdout/stderr inherited so the shell is usable.
    let mut child = cmd.spawn().context("spawning the interactive sandbox")?;

    // The shim reads its one request line off fd 0, then execs the command, which
    // inherits fd 0. So we must send the request line and then keep feeding the
    // command's stdin — NOT shut it down as the one-shot paths do. Closing it here
    // (the old bug) handed `bash -l` an immediate EOF, so `cowboy shell` exited at
    // once instead of giving an interactive shell. Write the request, then pump the
    // process's own stdin (the user's terminal) into the child for its lifetime.
    let mut child_stdin = child.stdin.take().context("sandbox stdin unavailable")?;
    let mut line = serde_json::to_vec(&request)?;
    debug_assert!(!line.contains(&b'\n'), "the request must be a single line");
    line.push(b'\n');
    child_stdin
        .write_all(&line)
        .await
        .context("sending the shim request")?;

    // Copy terminal stdin -> child stdin until either side ends; then the child's
    // stdin closes (EOF), which is the normal way an interactive shell exits on
    // Ctrl-D. Runs as a detached task so we can still await the child.
    let pump = tokio::spawn(async move {
        let mut term_stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut term_stdin, &mut child_stdin).await;
        let _ = child_stdin.shutdown().await;
    });

    let status = child.wait().await.context("waiting for the shell")?;
    // The shell exited; stop pumping (the copy task may still be blocked on a
    // terminal read). Aborting drops `child_stdin`, closing the pipe.
    pump.abort();
    Ok(ExecResult {
        exit_code: status.code().unwrap_or(-1),
    })
}

fn to_strings(paths: &[std::path::PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::SecurityConfig;
    use cowboy_sandbox::plan::PlanInputs;
    use cowboy_sandbox::probe::FakeHost;
    use std::path::{Path, PathBuf};

    fn dummy_plan() -> SandboxPlan {
        let probe = FakeHost::new().with_existing(["/usr", "/srv/proj"]);
        let sec = SecurityConfig::default();
        SandboxPlan::build(
            &PlanInputs {
                root: Path::new("/srv/proj"),
                security: &sec,
                grants: &[],
                mask_file: Path::new("/run/mask"),
                relay_port: 8443,
                scratch: Path::new("/scratch"),
                agent_home: Path::new("/cache/cowboy/home/proj"),
                git_identity: None,
            },
            &probe,
        )
        .unwrap()
    }

    /// Upgrading cowboy mid-session must say so, not fail as a missing file inside the
    /// sandbox.
    ///
    /// `cargo install` while a session runs replaces the binary, after which
    /// `current_exe()` reads `".../cowboy (deleted)"`. The plan bind-mounts that as the
    /// lockdown shim, binds render as `--ro-bind-try`, and a missing source is silently
    /// skipped — so bwrap reported `execvp /.cowboy-shim: No such file or directory`,
    /// naming the path *inside* the sandbox and saying nothing about the host binary
    /// that moved. Every command in the session failed that way: an agent alive and
    /// unable to run anything.
    ///
    /// `project::self_exe` resolves the `(deleted)` marker so this should not arise;
    /// this is the backstop that makes it legible if it ever does.
    #[test]
    fn a_missing_lockdown_shim_names_the_host_binary_not_the_sandbox_path() {
        // A plan whose shim source does not exist, exactly as a replaced binary leaves it.
        let probe = FakeHost {
            self_exe: Some(PathBuf::from("/cargo/bin/cowboy (deleted)")),
            ..FakeHost::new().with_existing(["/usr", "/srv/proj"])
        };
        let sec = SecurityConfig::default();
        let plan = SandboxPlan::build(
            &PlanInputs {
                root: Path::new("/srv/proj"),
                security: &sec,
                grants: &[],
                mask_file: Path::new("/run/mask"),
                relay_port: 8443,
                scratch: Path::new("/scratch"),
                agent_home: Path::new("/cache/cowboy/home/proj"),
                git_identity: None,
            },
            &probe,
        )
        .unwrap();

        let err = ensure_shim_is_bound(&plan).expect_err("a missing shim must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("/cargo/bin/cowboy (deleted)"),
            "the error must name the host path that went away: {msg}"
        );
        assert!(
            msg.contains("upgraded or moved"),
            "and say why that happens: {msg}"
        );
        assert!(
            !msg.contains(cowboy_sandbox::SHIM_PATH),
            "and not point at the path inside the sandbox, which is not the problem: {msg}"
        );

        // And a plan whose shim really is on disk passes.
        let live = FakeHost {
            self_exe: std::env::current_exe().ok(),
            ..FakeHost::new().with_existing(["/usr", "/srv/proj"])
        };
        let ok_plan = SandboxPlan::build(
            &PlanInputs {
                root: Path::new("/srv/proj"),
                security: &sec,
                grants: &[],
                mask_file: Path::new("/run/mask"),
                relay_port: 8443,
                scratch: Path::new("/scratch"),
                agent_home: Path::new("/cache/cowboy/home/proj"),
                git_identity: None,
            },
            &live,
        )
        .unwrap();
        assert!(ensure_shim_is_bound(&ok_plan).is_ok());
    }

    fn req<'a>(plan: &'a SandboxPlan, command: &'a str, cwd: Option<&'a str>) -> ExecRequest<'a> {
        ExecRequest {
            plan,
            command,
            cwd,
            timeout_secs: 0,
            net: NetMode::Isolated,
            session: None,
        }
    }

    #[test]
    fn no_cd_when_the_cwd_is_the_workdir() {
        let p = dummy_plan();
        assert_eq!(shell_command_for(&req(&p, "make", None)), "make");
        assert_eq!(
            shell_command_for(&req(&p, "make", Some(&p.workdir.clone()))),
            "make"
        );
    }

    #[test]
    fn cd_is_prepended_for_a_subdirectory() {
        let p = dummy_plan();
        assert_eq!(
            shell_command_for(&req(&p, "make", Some("/workspace/sub"))),
            "cd '/workspace/sub' && make"
        );
    }

    /// A cwd is data, not code. Without quoting, a crafted directory name would
    /// run in command position.
    #[test]
    fn cwd_cannot_inject_a_command() {
        let p = dummy_plan();
        let out = shell_command_for(&req(&p, "make", Some("/tmp/x'; rm -rf /; echo '")));
        assert!(
            !out.contains("; rm -rf /;") || out.starts_with("cd '/tmp/x'\\''"),
            "cwd must be quoted: {out}"
        );
        assert!(out.ends_with("&& make"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
