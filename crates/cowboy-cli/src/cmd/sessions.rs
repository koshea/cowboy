//! `cowboy sessions` — list sessions known to the daemon.

use anyhow::Result;
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, SessionInfo, SessionStatus};

use crate::cmd::daemon;
use crate::style;

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
    // Show the ranch/workstream column only when some session has one.
    let any_ranch = sessions.iter().any(|s| s.ranch_id.is_some());
    if any_ranch {
        println!(
            "{}",
            style::bold(&format!(
                "{:<22} {:<11} {:<18} {:<22} TASK",
                "ID", "STATUS", "BRANCH", "RANCH/WORKSTREAM"
            ))
        );
        for s in &sessions {
            println!(
                "{:<22} {} {:<18} {:<22} {}",
                s.id,
                status_cell(s.status),
                s.branch.as_deref().unwrap_or("-"),
                truncate(&ranch_cell(s), 22),
                task_str(s),
            );
        }
    } else {
        println!(
            "{}",
            style::bold(&format!(
                "{:<22} {:<11} {:<18} {:<34} TASK",
                "ID", "STATUS", "BRANCH", "WORKTREE"
            ))
        );
        for s in &sessions {
            println!(
                "{:<22} {} {:<18} {:<34} {}",
                s.id,
                status_cell(s.status),
                s.branch.as_deref().unwrap_or("-"),
                truncate(&s.root.display().to_string(), 34),
                task_str(s),
            );
        }
    }
    Ok(())
}

/// "ranch/workstream" for a session, or "-".
fn ranch_cell(s: &SessionInfo) -> String {
    match (&s.ranch_id, &s.workstream_id) {
        (Some(r), Some(w)) => format!("{r}/{w}"),
        (Some(r), None) => r.clone(),
        _ => "-".to_string(),
    }
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
    println!(
        "{}",
        style::success(&format!("{verb} {} stale session(s):", reclaimed.len()))
    );
    for id in &reclaimed {
        println!("  {}", style::dim(id));
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

/// The status column, padded to width *then* colored by state (so the ANSI codes
/// don't throw off alignment, and piped/non-TTY output stays plain).
fn status_cell(s: SessionStatus) -> String {
    let padded = format!("{:<11}", status_str(s));
    match s {
        SessionStatus::Running | SessionStatus::Idle => style::green(&padded),
        SessionStatus::Starting => style::cyan(&padded),
        SessionStatus::AwaitingApproval | SessionStatus::AwaitingInput | SessionStatus::Blocked => {
            style::yellow(&padded)
        }
        SessionStatus::Failed | SessionStatus::Stale => style::red(&padded),
        SessionStatus::Completed => style::dim(&padded),
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
