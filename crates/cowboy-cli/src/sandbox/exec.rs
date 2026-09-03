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
const EXIT_TIMEOUT: i32 = 124;
/// Exit code reported for a command the user interrupted.
const EXIT_CANCELLED: i32 = 130;
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
        scope_ipc: plan.landlock.scope_ipc,
        deny_syscalls: plan.seccomp.denied.iter().map(|s| s.to_string()).collect(),
        deny_raw_sockets: plan.seccomp.deny_raw_sockets,
    };
    Ok((cmd, request))
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
    let (mut cmd, request) = build_command(plan, command, net, session)?;
    cmd.stdin(Stdio::piped());
    // stdout/stderr inherited so the shell is usable.
    let mut child = cmd.spawn().context("spawning the interactive sandbox")?;
    send_request(&mut child, &request, None).await?;
    let status = child.wait().await.context("waiting for the shell")?;
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
    use std::path::Path;

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
            },
            &probe,
        )
        .unwrap()
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
