//! `cowboy down` — end this project's sessions (or every session) and reap residue.
//!
//! Much smaller than it was. Tearing down a container meant removing named objects
//! that outlived the process which created them: an agent container, a gateway
//! sidecar, and a bridge network, all reconstructed from a hash of the worktree path
//! so that three different callers could agree on what to delete.
//!
//! A sandbox has no such objects. Its namespaces, interception ruleset and resource
//! cgroup are all owned by a holder process whose lifetime is tied to the worker's,
//! so ending the worker *is* the teardown — including after a crash, which is why
//! the daemon no longer needs a reaper for it either.
//!
//! What can outlive a worker is an empty cgroup **directory**: the kernel keeps it
//! until someone removes it, and only a clean shutdown does. So this reaps those too
//! — harmless, but they accumulate one per crashed session.

use std::path::Path;

use anyhow::Result;
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, SessionInfo};

use crate::cli::DownArgs;

/// Stop the worker processes of the given live sessions (SIGTERM).
///
/// This is the whole teardown: the worker's exit closes the holder's stdin, the
/// holder exits, and the kernel releases the namespaces — taking any process still
/// running inside them with it. Returns the count.
fn kill_session_workers(sessions: &[SessionInfo]) -> usize {
    let mut killed = 0;
    for s in sessions {
        if s.status.is_terminal() {
            continue;
        }
        if let Some(pid) = s.pid {
            // SAFETY: kill(pid, SIGTERM) is always safe; ESRCH if already gone.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            killed += 1;
        }
    }
    killed
}

/// Live sessions known to the daemon, optionally filtered to one worktree `root`.
/// Empty if the daemon isn't running.
async fn live_sessions(root: Option<&Path>) -> Vec<SessionInfo> {
    match crate::cmd::daemon::request(DaemonReq::ListSessions {
        root: root.map(Path::to_path_buf),
    })
    .await
    {
        Ok(DaemonResp::Sessions { sessions }) => sessions,
        _ => Vec::new(),
    }
}

pub async fn run(args: DownArgs) -> Result<()> {
    let (scope, sessions) = if args.all {
        ("every project", live_sessions(None).await)
    } else {
        let root = crate::cmd::project_root()?;
        ("this project", live_sessions(Some(&root)).await)
    };

    // `--all` reaches outside the project you are standing in, so the count is shown
    // *before* the SIGTERM — "stopped 7 session(s) in every project" is a bad way to
    // learn that six of them belonged to someone else's work.
    let live = sessions.iter().filter(|s| !s.status.is_terminal()).count();
    if args.all && live > 0 {
        crate::ui::warn(&format!(
            "{live} live session(s) across every project will be stopped:"
        ));
        for s in sessions.iter().filter(|s| !s.status.is_terminal()) {
            crate::ui::kv(&s.id, &s.root.display().to_string());
        }
        if !crate::prompt::confirm_destructive("Stop them all?")? {
            return Ok(());
        }
    }

    let killed = kill_session_workers(&sessions);

    // Give the workers a moment to exit so their cgroups are empty and removable.
    if killed > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    #[cfg(target_os = "linux")]
    let reaped = crate::sandbox::cgroup::reap_empty();
    // No cgroups on macOS; each worker's session sweeps its own processes as it exits.
    #[cfg(not(target_os = "linux"))]
    let reaped = 0;

    let mut msg = format!("stopped {killed} session(s) in {scope}");
    if reaped > 0 {
        msg.push_str(&format!(", reaped {reaped} leftover cgroup(s)"));
    }
    crate::ui::ok(&msg);
    Ok(())
}
