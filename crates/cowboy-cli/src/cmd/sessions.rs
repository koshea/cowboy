//! `cowboy sessions` — list sessions known to the daemon, optionally merged
//! with the current project's on-disk history.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, SessionInfo, SessionStatus};

use crate::cmd::daemon;
use crate::session::replay::DiskSession;
use crate::style;

pub async fn run(all: bool) -> Result<()> {
    if !all {
        return run_daemon_list().await;
    }

    let root = crate::cmd::project_root()?;
    let daemon_result = daemon::request(DaemonReq::ListSessions { root: None }).await;
    let (daemon_sessions, daemon_available) = match daemon_result {
        Ok(DaemonResp::Sessions { sessions }) => (sessions, true),
        Ok(other) => anyhow::bail!("unexpected daemon response: {other:?}"),
        Err(_) => (Vec::new(), false),
    };
    let disk_sessions = crate::session::replay::disk_sessions(&root)?;
    let rows = merge_rows(daemon_sessions, disk_sessions, &root);

    println!(
        "daemon scope: all known projects · disk history: current project only ({})",
        root.display()
    );
    if !daemon_available {
        println!("cowboyd unavailable; showing current-project disk history");
    }
    if rows.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    println!(
        "{}",
        style::bold(&format!(
            "{:<22} {:<11} {:<20} {:<18} {:<22} PROJECT / TASK",
            "ID", "STATUS", "ACTIVITY", "ATTENTION", "SOURCE / ACTION"
        ))
    );
    for row in rows {
        println!(
            "{:<22} {} {:<20} {:<18} {:<22} {}",
            row.id,
            row.status_cell(),
            truncate(&row.activity, 20),
            truncate(&row.attention, 18),
            row.source_action(),
            truncate(&row.project_task(), 46),
        );
    }
    Ok(())
}

/// Preserve the original daemon-only command and output for bare `sessions`.
async fn run_daemon_list() -> Result<()> {
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
    let any_ranch = sessions.iter().any(|session| session.ranch_id.is_some());
    if any_ranch {
        println!(
            "{}",
            style::bold(&format!(
                "{:<22} {:<11} {:<18} {:<22} TASK",
                "ID", "STATUS", "BRANCH", "RANCH/WORKSTREAM"
            ))
        );
        for session in &sessions {
            println!(
                "{:<22} {} {:<18} {:<22} {}",
                session.id,
                status_cell(session.status),
                session.branch.as_deref().unwrap_or("-"),
                truncate(&ranch_cell(session), 22),
                task_str(session),
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
        for session in &sessions {
            println!(
                "{:<22} {} {:<18} {:<34} {}",
                session.id,
                status_cell(session.status),
                session.branch.as_deref().unwrap_or("-"),
                truncate(&session.root.display().to_string(), 34),
                task_str(session),
            );
        }
    }
    Ok(())
}

#[derive(Debug)]
struct BrowserRow {
    id: String,
    root: PathBuf,
    daemon: Option<SessionInfo>,
    disk: Option<DiskSession>,
    started_ms: u64,
    activity: String,
    attention: String,
}

impl BrowserRow {
    fn status_cell(&self) -> String {
        self.daemon.as_ref().map_or_else(
            || style::dim(&format!("{:<11}", "saved")),
            |session| status_cell(session.status),
        )
    }

    fn source_action(&self) -> String {
        let action = match self.daemon.as_ref().map(|session| session.status) {
            Some(
                SessionStatus::Starting
                | SessionStatus::Running
                | SessionStatus::Idle
                | SessionStatus::AwaitingApproval
                | SessionStatus::AwaitingInput
                | SessionStatus::Blocked,
            ) => "attach",
            _ => "replay",
        };
        match (&self.daemon, &self.disk) {
            (Some(_), Some(_)) => format!("daemon+disk → {action}"),
            (Some(_), None) => format!("daemon → {action}"),
            (None, Some(_)) => "current disk → replay".to_string(),
            (None, None) => unreachable!("a browser row must have a source"),
        }
    }

    fn project_task(&self) -> String {
        let root = self.root.display();
        if let Some(task) = self
            .daemon
            .as_ref()
            .and_then(|session| session.task.as_deref())
            .filter(|task| !task.is_empty())
        {
            return format!("{root} · {task}");
        }
        match self.disk.as_ref().map(|disk| disk.summary.as_str()) {
            Some(summary) if summary != "(no final summary)" => format!("{root} · {summary}"),
            _ => format!("{root}"),
        }
    }
}

fn merge_rows(
    daemon_sessions: Vec<SessionInfo>,
    disk_sessions: Vec<DiskSession>,
    disk_root: &Path,
) -> Vec<BrowserRow> {
    let mut rows: HashMap<(PathBuf, String), BrowserRow> = HashMap::new();
    for disk in disk_sessions {
        let root = normalized_root(disk_root);
        let key = (root.clone(), disk.id.clone());
        rows.insert(
            key,
            BrowserRow {
                id: disk.id.clone(),
                root,
                daemon: None,
                started_ms: disk.started_ms,
                activity: disk.activity.clone(),
                attention: disk.attention.clone(),
                disk: Some(disk),
            },
        );
    }
    for session in daemon_sessions {
        let root = normalized_root(&session.root);
        let key = (root.clone(), session.id.clone());
        let disk = rows.remove(&key).and_then(|row| row.disk);
        let started_ms = if session.started_ms > 0 {
            session.started_ms
        } else {
            disk.as_ref()
                .map_or_else(|| id_started_ms(&session.id), |disk| disk.started_ms)
        };
        let activity = daemon_activity(&session, disk.as_ref());
        let attention = daemon_attention(&session);
        rows.insert(
            key,
            BrowserRow {
                id: session.id.clone(),
                root,
                daemon: Some(session),
                disk,
                started_ms,
                activity,
                attention,
            },
        );
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|a, b| {
        b.started_ms
            .cmp(&a.started_ms)
            .then_with(|| b.id.cmp(&a.id))
            .then_with(|| b.root.cmp(&a.root))
    });
    rows
}

fn daemon_activity(session: &SessionInfo, disk: Option<&DiskSession>) -> String {
    if let Some(command) = session.running_command.as_deref() {
        return format!("exec {command}");
    }
    if session.turn > 0 {
        return format!("turn {}", session.turn);
    }
    if !session.diffstat.is_empty() {
        return session.diffstat.clone();
    }
    disk.map_or_else(|| "-".to_string(), |disk| disk.activity.clone())
}

fn daemon_attention(session: &SessionInfo) -> String {
    match session.status {
        SessionStatus::AwaitingApproval => "approval needed".to_string(),
        SessionStatus::AwaitingInput => "input needed".to_string(),
        SessionStatus::Blocked => session
            .blocked_reason
            .clone()
            .unwrap_or_else(|| "blocked".to_string()),
        SessionStatus::Failed => "failed".to_string(),
        SessionStatus::Stale => "stale".to_string(),
        _ => "-".to_string(),
    }
}

fn normalized_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn id_started_ms(id: &str) -> u64 {
    id.split_once('-')
        .map_or(id, |(timestamp, _)| timestamp)
        .parse()
        .unwrap_or(0)
}

/// "ranch/workstream" for a session, or "-".
fn ranch_cell(session: &SessionInfo) -> String {
    match (&session.ranch_id, &session.workstream_id) {
        (Some(ranch), Some(workstream)) => format!("{ranch}/{workstream}"),
        (Some(ranch), None) => ranch.clone(),
        _ => "-".to_string(),
    }
}

/// `cowboy session cleanup [--dry-run]` — reap stale session records and
/// release their leases. Worktrees and branches are never touched.
pub async fn cleanup(dry_run: bool) -> Result<()> {
    let resp = match daemon::request(DaemonReq::CleanupStale { dry_run }).await {
        Ok(response) => response,
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
        "\nworktrees and branches are left untouched; end orphaned sessions \
         with `cowboy down`."
    );
    Ok(())
}

fn status_str(status: SessionStatus) -> &'static str {
    match status {
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
fn status_cell(status: SessionStatus) -> String {
    let padded = format!("{:<11}", status_str(status));
    match status {
        SessionStatus::Running | SessionStatus::Idle => style::green(&padded),
        SessionStatus::Starting => style::cyan(&padded),
        SessionStatus::AwaitingApproval | SessionStatus::AwaitingInput | SessionStatus::Blocked => {
            style::yellow(&padded)
        }
        SessionStatus::Failed | SessionStatus::Stale => style::red(&padded),
        SessionStatus::Completed => style::dim(&padded),
    }
}

fn task_str(session: &SessionInfo) -> String {
    // While blocked, surface the reason in place of the task so it's visible.
    if session.status == SessionStatus::Blocked {
        if let Some(reason) = &session.blocked_reason {
            return format!("⏸ {reason}");
        }
    }
    session.task.clone().unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_session(root: &Path, id: &str, status: SessionStatus) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            root: root.into(),
            task: Some("daemon task".into()),
            status,
            pid: None,
            branch: None,
            session_name: None,
            worker_sock: None,
            journal_path: None,
            lease_mode: None,
            started_ms: id_started_ms(id),
            last_heartbeat_ms: 0,
            turn: 2,
            tokens: (0, 0),
            attached_clients: 0,
            diffstat: String::new(),
            running_command: None,
            blocked_reason: None,
            ranch_id: None,
            workstream_id: None,
        }
    }

    fn disk_session(id: &str) -> DiskSession {
        DiskSession {
            id: id.into(),
            started_ms: id_started_ms(id),
            activity: "3 msg · 1 cmd".into(),
            attention: "incomplete".into(),
            summary: "disk summary".into(),
        }
    }

    #[test]
    fn merged_rows_deduplicate_by_root_and_id_with_daemon_authoritative() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut live = daemon_session(tmp.path(), "200-1", SessionStatus::AwaitingInput);
        live.turn = 7;
        let rows = merge_rows(vec![live], vec![disk_session("200-1")], tmp.path());

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].daemon.as_ref().unwrap().status,
            SessionStatus::AwaitingInput
        );
        assert!(rows[0].disk.is_some());
        assert_eq!(rows[0].activity, "turn 7");
        assert_eq!(rows[0].attention, "input needed");
        assert_eq!(rows[0].source_action(), "daemon+disk → attach");
    }

    #[test]
    fn merged_rows_keep_same_id_from_different_roots_and_sort_newest_first() {
        let current = assert_fs::TempDir::new().unwrap();
        let other = assert_fs::TempDir::new().unwrap();
        let rows = merge_rows(
            vec![
                daemon_session(other.path(), "100-1", SessionStatus::Completed),
                daemon_session(other.path(), "300-1", SessionStatus::Running),
            ],
            vec![disk_session("100-1"), disk_session("200-1")],
            current.path(),
        );

        assert_eq!(
            rows.iter()
                .take(2)
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["300-1", "200-1"]
        );
        assert_eq!(rows[1].source_action(), "current disk → replay");
        assert!(rows[1].project_task().ends_with(" · disk summary"));
        let mut duplicate_sources: Vec<_> = rows
            .iter()
            .filter(|row| row.id == "100-1")
            .map(BrowserRow::source_action)
            .collect();
        duplicate_sources.sort();
        assert_eq!(
            duplicate_sources,
            ["current disk → replay", "daemon → replay"]
        );
    }
}
