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

/// `cowboy session cleanup [--dry-run]` — reap stale session records and
/// release their leases. Worktrees and branches are never touched.
pub async fn cleanup(dry_run: bool) -> Result<()> {
    let resp = match daemon::request(DaemonReq::CleanupStale { dry_run }).await {
        Ok(r) => r,
        Err(_) => {
            println!("nothing to clean up (cowboyd not running)");
            return Ok(());
        }
    };
    let (reclaimed, leases_released) = match resp {
        DaemonResp::CleanedUp {
            reclaimed,
            leases_released,
        } => (reclaimed, leases_released),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    };
    if reclaimed.is_empty() {
        println!("no stale sessions to reap");
        return Ok(());
    }
    let verb = if dry_run { "would reap" } else { "reaped" };
    println!("{verb} {} stale session(s):", reclaimed.len());
    for id in &reclaimed {
        println!("  {id}");
    }
    if !leases_released.is_empty() {
        let verb = if dry_run { "would free" } else { "freed" };
        println!("{verb} {} worktree lease(s):", leases_released.len());
        for key in &leases_released {
            println!("  {}", key.display());
        }
    }
    println!(
        "\nworktrees and branches are left untouched; remove orphaned containers \
         with `cowboy down`."
    );
    Ok(())
}

fn status_str(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::AwaitingApproval => "approval",
        SessionStatus::AwaitingInput => "input",
        SessionStatus::Blocked => "blocked",
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Stale => "stale",
    }
}

fn task_str(s: &SessionInfo) -> String {
    // While blocked, surface the reason in place of the task so it's visible.
    if s.status == SessionStatus::Blocked {
        if let Some(r) = &s.blocked_reason {
            return format!("⏸ {r}");
        }
    }
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
