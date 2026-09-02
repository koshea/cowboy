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
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use cowboy_sandbox::SandboxPlan;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::bwrap::{self, NetMode};
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
    let bwrap_path = bwrap::resolve_bwrap()?;

    // The shim runs from inside the sandbox at a fixed path; the plan binds the
    // cowboy binary there (see `cowboy_sandbox::SHIM_PATH`).
    let shim_argv: Vec<OsString> = vec![cowboy_sandbox::SHIM_PATH.into(), "x-sandbox-shim".into()];
    let argv = bwrap::build_argv(&bwrap_path, req.plan, req.net, &shim_argv);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Combined output on one pipe keeps interleaving faithful to what the command
    // actually produced; two pipes would reorder it. No PTY: over a plain pipe,
    // tools like cargo and mise emit plain streamable lines instead of
    // cursor-movement progress we would have to emulate.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Its own process group, so a signal aimed at the command cannot reach cowboy.
    cmd.process_group(0);
    // Reap on drop as a backstop; the explicit kill path below is the normal one.
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", bwrap_path.display()))?;

    // The shim reads its instructions from stdin rather than argv, so they are not
    // exposed in /proc/<pid>/cmdline.
    let request = ShimRequest {
        command: build_command(&req),
        read_only: to_strings(&req.plan.landlock.read_only),
        read_write: to_strings(&req.plan.landlock.read_write),
        connect_tcp: req.plan.landlock.connect_tcp.clone(),
        scope_ipc: req.plan.landlock.scope_ipc,
        deny_syscalls: req
            .plan
            .seccomp
            .denied
            .iter()
            .map(|s| s.to_string())
            .collect(),
        deny_raw_sockets: req.plan.seccomp.deny_raw_sockets,
    };
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().context("sandbox stdin unavailable")?;
        let json = serde_json::to_vec(&request)?;
        stdin
            .write_all(&json)
            .await
            .context("sending the shim request")?;
        // Closing stdin is what lets the shim's read_to_string return.
        stdin.shutdown().await.ok();
    }

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
fn build_command(req: &ExecRequest<'_>) -> String {
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

fn to_strings(paths: &[std::path::PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Build a plan for a one-off command in `root`, for `cowboy sandbox exec`.
pub fn plan_for(
    root: &Path,
    security: &cowboy_core::config::SecurityConfig,
    mask_file: &Path,
    probe: &dyn cowboy_sandbox::HostProbe,
) -> Result<SandboxPlan> {
    use cowboy_sandbox::plan::PlanInputs;
    let inputs = PlanInputs {
        root,
        security,
        grants: &[],
        mask_file,
        relay_port: super::RELAY_PORT,
    };
    SandboxPlan::build(&inputs, probe).map_err(anyhow::Error::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::SecurityConfig;
    use cowboy_sandbox::plan::PlanInputs;
    use cowboy_sandbox::probe::FakeHost;

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
        }
    }

    #[test]
    fn no_cd_when_the_cwd_is_the_workdir() {
        let p = dummy_plan();
        assert_eq!(build_command(&req(&p, "make", None)), "make");
        assert_eq!(
            build_command(&req(&p, "make", Some(&p.workdir.clone()))),
            "make"
        );
    }

    #[test]
    fn cd_is_prepended_for_a_subdirectory() {
        let p = dummy_plan();
        assert_eq!(
            build_command(&req(&p, "make", Some("/workspace/sub"))),
            "cd '/workspace/sub' && make"
        );
    }

    /// A cwd is data, not code. Without quoting, a crafted directory name would
    /// run in command position.
    #[test]
    fn cwd_cannot_inject_a_command() {
        let p = dummy_plan();
        let out = build_command(&req(&p, "make", Some("/tmp/x'; rm -rf /; echo '")));
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
