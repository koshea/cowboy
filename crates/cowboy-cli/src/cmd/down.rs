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
use crate::style;

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
    let (scope, killed) = if args.all {
        (
            "every project",
            kill_session_workers(&live_sessions(None).await),
        )
    } else {
        let root = crate::cmd::project_root()?;
        (
            "this project",
            kill_session_workers(&live_sessions(Some(&root)).await),
        )
    };

    // Give the workers a moment to exit so their cgroups are empty and removable.
    if killed > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let reaped = crate::sandbox::cgroup::reap_empty();

    let mut msg = format!("cowboy down: stopped {killed} session(s) in {scope}");
    if reaped > 0 {
        msg.push_str(&format!(", reaped {reaped} leftover cgroup(s)"));
    }
    println!("{}", style::success(&msg));
    Ok(())
}
