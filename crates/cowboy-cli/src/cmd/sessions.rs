//! `cowboy sessions` — list sessions known to the daemon.

use anyhow::Result;
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, SessionInfo, SessionStatus};

use crate::cmd::daemon;

pub async fn run() -> Result<()> {
    let sessions = match daemon::request(DaemonReq::ListSessions { root: None }).await {
        Ok(DaemonResp::Sessions { sessions }) => sessions,
        // No daemon running => no sessions.
        Err(_) => {
            println!("no sessions (cowboyd not running)");
            return Ok(());
        }
        Ok(other) => anyhow::bail!("unexpected daemon response: {other:?}"),
    };
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!(
        "{:<22} {:<11} {:<18} {:<34} TASK",
        "ID", "STATUS", "BRANCH", "WORKTREE"
    );
    for s in &sessions {
        println!(
            "{:<22} {:<11} {:<18} {:<34} {}",
            s.id,
            status_str(s.status),
            s.branch.as_deref().unwrap_or("-"),
            truncate(&s.root.display().to_string(), 34),
            task_str(s),
        );
    }
    Ok(())
}

fn status_str(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::AwaitingApproval => "approval",
        SessionStatus::AwaitingInput => "input",
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Stale => "stale",
    }
}

fn task_str(s: &SessionInfo) -> String {
    s.task.clone().unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
