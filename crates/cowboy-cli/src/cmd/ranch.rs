//! `cowboy ranch` — create and inspect Ranch Plans (multi-workstream tasks).
//!
//! The plan lives at `.cowboy/ranches/<id>/ranch.yaml` and is committed (the
//! shared source of truth). `create` writes a skeleton to fill in; launching
//! workstreams arrives in a later stage.

use anyhow::{bail, Context, Result};
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, LeaseMode, SessionStatus};
use cowboy_core::ranch::{self, Ranch, RanchStatus, WorkstreamStatus};

use crate::cli::RanchCommand;
use crate::cmd::daemon;

pub async fn run(command: RanchCommand) -> Result<()> {
    let root = crate::cmd::project_root()?;
    match command {
        RanchCommand::Create { title, goal } => create(&root, &title, goal),
        RanchCommand::Status { id } => status(&root, id),
        RanchCommand::Start { id } => start(&root, &id).await,
    }
}

fn create(root: &std::path::Path, title: &str, goal: Option<String>) -> Result<()> {
    let id = ranch::fresh_id(root, title);
    let now = now_ms();
    let goal = goal.unwrap_or_else(|| "(describe the overall goal)".into());
    // Write a templated skeleton (comments guide editing; serde ignores them).
    let yaml = format!(
        "version: 1\n\
         id: {id}\n\
         title: {title:?}\n\
         goal: {goal:?}\n\
         status: planning\n\
         created_ms: {now}\n\
         updated_ms: {now}\n\
         workstreams: []\n\
         # Define the workstreams to run, e.g.:\n\
         # workstreams:\n\
         #   - id: schema\n\
         #     title: Add billing schema\n\
         #     goal: Add tables + migrations for billing.\n\
         #     depends_on: []\n\
         #     expected_artifacts: [schema-contract.md]\n\
         #     acceptance:\n\
         #       - migrations apply cleanly\n\
         #   - id: api\n\
         #     title: Implement billing API\n\
         #     depends_on: [schema]\n\
         #     expected_artifacts: [api-contract.md]\n"
    );
    let path = ranch::ranch_path(root, &id);
    std::fs::create_dir_all(path.parent().unwrap())
        .with_context(|| format!("creating {}", path.display()))?;
    std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;
    // Validate it parses.
    ranch::load(root, &id).context("the new ranch.yaml should parse")?;
    println!("✓ created ranch `{id}` at {}", path.display());
    println!("  edit it to add workstreams (id, goal, depends_on, acceptance),");
    println!("  then check it with `cowboy ranch status {id}`.");
    Ok(())
}

fn status(root: &std::path::Path, id: Option<String>) -> Result<()> {
    match id {
        Some(id) => show_one(root, &id),
        None => {
            let ranches = ranch::list(root);
            if ranches.is_empty() {
                println!("no ranches (create one with `cowboy ranch create \"<title>\"`)");
                return Ok(());
            }
            println!("{:<20} {:<12} WORKSTREAMS  TITLE", "ID", "STATUS");
            for r in &ranches {
                println!(
                    "{:<20} {:<12} {:<12} {}",
                    r.id,
                    ranch_status(r.status),
                    r.workstreams.len(),
                    r.title
                );
            }
            Ok(())
        }
    }
}

fn show_one(root: &std::path::Path, id: &str) -> Result<()> {
    let mut r = ranch::load(root, id)?;
    // Reflect the live dependency graph in the displayed statuses.
    r.recompute_readiness();
    println!("ranch {} — {}", r.id, r.title);
    println!("status: {}", ranch_status(r.status));
    if !r.goal.is_empty() {
        println!("goal:   {}", r.goal);
    }
    if r.workstreams.is_empty() {
        println!(
            "\n(no workstreams yet — edit {})",
            ranch::ranch_path(root, id).display()
        );
        return Ok(());
    }
    println!(
        "\n{:<16} {:<12} {:<16} DEPENDS ON",
        "WORKSTREAM", "STATUS", "SESSION"
    );
    for w in &r.workstreams {
        println!(
            "{:<16} {:<12} {:<16} {}",
            w.id,
            ws_status(w.status),
            w.session_id.as_deref().unwrap_or("-"),
            w.depends_on.join(", ")
        );
    }
    let ready: Vec<_> = r.ready_workstreams().iter().map(|w| w.id.clone()).collect();
    if !ready.is_empty() {
        println!("\nready to start: {}", ready.join(", "));
    }
    Ok(())
}

/// `cowboy ranch start <id>` — reconcile finished workstreams, then launch every
/// newly-ready one in its own worktree/branch. Idempotent + re-entrant: run it
/// again as workstreams complete to advance the dependency graph.
async fn start(root: &std::path::Path, id: &str) -> Result<()> {
    daemon::ensure_running().await?;
    let mut ranch = ranch::load(root, id)?;

    // Look up the live status of each already-started workstream's session.
    let mut session_status: std::collections::HashMap<String, SessionStatus> = Default::default();
    for w in &ranch.workstreams {
        if let Some(sid) = &w.session_id {
            if let Ok(DaemonResp::Session { info }) =
                daemon::request(DaemonReq::GetSession { id: sid.clone() }).await
            {
                session_status.insert(sid.clone(), info.status);
            }
        }
    }
    let to_start = reconcile_and_pick(&mut ranch, &|sid| session_status.get(sid).copied());

    let mut started: Vec<(String, String, String)> = Vec::new();
    for ws_id in &to_start {
        let ws = ranch.workstream(ws_id).unwrap().clone();
        let branch = format!("cowboy/{}-{}", ranch.id, ws.id);
        let (path, branch) = match daemon::request(DaemonReq::CreateWorktree {
            repo: root.to_path_buf(),
            branch,
            path: None,
        })
        .await?
        {
            DaemonResp::WorktreeCreated { path, branch } => (path, branch),
            DaemonResp::Err { message } => {
                eprintln!("skip {}: worktree: {message}", ws.id);
                continue;
            }
            other => bail!("unexpected daemon response: {other:?}"),
        };
        let task = compose_task(&ranch, &ws);
        match daemon::request(DaemonReq::StartSession {
            root: path.clone(),
            task: Some(task),
            mode: LeaseMode::Exclusive,
            force: false,
            resume: None,
            ranch_id: Some(ranch.id.clone()),
            workstream_id: Some(ws.id.clone()),
        })
        .await?
        {
            DaemonResp::Started { id: sid, .. } => {
                let w = ranch.workstream_mut(ws_id).unwrap();
                w.status = WorkstreamStatus::Running;
                w.session_id = Some(sid.clone());
                w.branch = Some(branch.clone());
                w.worktree_path = Some(path.clone());
                started.push((ws_id.clone(), sid, branch));
            }
            DaemonResp::LeaseDenied { .. } => eprintln!("skip {}: worktree already in use", ws.id),
            DaemonResp::Err { message } => eprintln!("skip {}: {message}", ws.id),
            other => bail!("unexpected daemon response: {other:?}"),
        }
    }

    // Reflect overall ranch state.
    if !ranch.workstreams.is_empty() && ranch.workstreams.iter().all(|w| w.status.is_done()) {
        ranch.status = RanchStatus::Complete;
    } else if ranch.workstreams.iter().any(|w| {
        matches!(
            w.status,
            WorkstreamStatus::Running | WorkstreamStatus::Starting
        )
    }) {
        ranch.status = RanchStatus::Running;
    }
    ranch.updated_ms = now_ms();
    ranch::save(root, &ranch)?;

    if started.is_empty() {
        println!("nothing ready to start.");
    } else {
        println!("started {} workstream(s):", started.len());
        for (wid, sid, branch) in &started {
            println!("  {wid}  → session {sid}  on {branch}");
        }
    }
    let blocked: Vec<_> = ranch
        .workstreams
        .iter()
        .filter(|w| w.status == WorkstreamStatus::Blocked)
        .map(|w| w.id.clone())
        .collect();
    if !blocked.is_empty() {
        println!("still blocked: {}", blocked.join(", "));
        println!(
            "re-run `cowboy ranch start {}` as workstreams complete.",
            ranch.id
        );
    }
    if ranch.status == RanchStatus::Complete {
        println!("ranch complete 🎉");
    }
    Ok(())
}

/// Reconcile already-started workstreams from their session status, recompute
/// readiness, and return the ids now ready to launch. Pure (clock/IO injected),
/// so it's unit-testable without a daemon.
fn reconcile_and_pick(
    ranch: &mut Ranch,
    session_status: &dyn Fn(&str) -> Option<SessionStatus>,
) -> Vec<String> {
    for w in &mut ranch.workstreams {
        if matches!(
            w.status,
            WorkstreamStatus::Running | WorkstreamStatus::Starting
        ) {
            if let Some(sid) = &w.session_id {
                match session_status(sid) {
                    Some(SessionStatus::Completed) => w.status = WorkstreamStatus::Complete,
                    Some(SessionStatus::Failed) | Some(SessionStatus::Stale) => {
                        w.status = WorkstreamStatus::Failed
                    }
                    _ => {}
                }
            }
        }
    }
    ranch.recompute_readiness();
    ranch
        .workstreams
        .iter()
        .filter(|w| w.status == WorkstreamStatus::Ready)
        .map(|w| w.id.clone())
        .collect()
}

/// Build the worker task prompt for a workstream (Stage 3 will inject the actual
/// dependency artifacts; for now it states the goal, acceptance, and the
/// coordination rules).
fn compose_task(ranch: &Ranch, ws: &cowboy_core::ranch::Workstream) -> String {
    let mut s = format!(
        "You are running ONE workstream of a larger Ranch Plan.\n\nRanch: {}\n",
        ranch.title
    );
    if !ranch.goal.is_empty() {
        s.push_str(&format!("Ranch goal: {}\n", ranch.goal));
    }
    s.push_str(&format!("\nYour workstream: {} ({})\n", ws.title, ws.id));
    if !ws.goal.is_empty() {
        s.push_str(&format!("{}\n", ws.goal));
    }
    if !ws.depends_on.is_empty() {
        s.push_str(&format!(
            "\nDepends on (complete): {}\n",
            ws.depends_on.join(", ")
        ));
    }
    if !ws.expected_artifacts.is_empty() {
        s.push_str(&format!(
            "Expected artifacts to publish: {}\n",
            ws.expected_artifacts.join(", ")
        ));
    }
    if !ws.acceptance.is_empty() {
        s.push_str("\nAcceptance criteria:\n");
        for a in &ws.acceptance {
            s.push_str(&format!("- {a}\n"));
        }
    }
    s.push_str(
        "\nCoordination rules:\n\
         - Work only on this workstream, in this worktree.\n\
         - Publish status/blockers/outputs with your tools (artifact / blocked / handoff).\n\
         - Do NOT edit the ranch plan; if it looks wrong, say so and stop rather than diverging.\n\
         - When done, publish the expected artifacts and a handoff, then finish.\n",
    );
    s
}

fn ranch_status(s: RanchStatus) -> &'static str {
    match s {
        RanchStatus::Planning => "planning",
        RanchStatus::Ready => "ready",
        RanchStatus::Running => "running",
        RanchStatus::WaitingForUser => "waiting",
        RanchStatus::Paused => "paused",
        RanchStatus::Integrating => "integrating",
        RanchStatus::Complete => "complete",
        RanchStatus::Failed => "failed",
        RanchStatus::Cancelled => "cancelled",
    }
}

fn ws_status(s: WorkstreamStatus) -> &'static str {
    match s {
        WorkstreamStatus::Planned => "planned",
        WorkstreamStatus::Blocked => "blocked",
        WorkstreamStatus::Ready => "ready",
        WorkstreamStatus::Starting => "starting",
        WorkstreamStatus::Running => "running",
        WorkstreamStatus::WaitingForUser => "waiting",
        WorkstreamStatus::Complete => "complete",
        WorkstreamStatus::Failed => "failed",
        WorkstreamStatus::Cancelled => "cancelled",
        WorkstreamStatus::MergeReady => "merge-ready",
        WorkstreamStatus::Integrated => "integrated",
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::ranch::Workstream;

    fn ws(id: &str, deps: &[&str], status: WorkstreamStatus, session: Option<&str>) -> Workstream {
        Workstream {
            id: id.into(),
            title: id.to_uppercase(),
            goal: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            status,
            session_id: session.map(|s| s.to_string()),
            branch: None,
            worktree_path: None,
            expected_artifacts: vec![],
            acceptance: vec![],
        }
    }

    fn ranch(ws: Vec<Workstream>) -> Ranch {
        Ranch {
            version: 1,
            id: "r".into(),
            title: "R".into(),
            goal: String::new(),
            status: RanchStatus::Running,
            workstreams: ws,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn reconcile_marks_finished_and_picks_newly_ready() {
        // schema is Running on session s1; api waits on schema; ui waits on api.
        let mut r = ranch(vec![
            ws("schema", &[], WorkstreamStatus::Running, Some("s1")),
            ws("api", &["schema"], WorkstreamStatus::Planned, None),
            ws("ui", &["api"], WorkstreamStatus::Planned, None),
        ]);
        // s1 has Completed → schema becomes Complete, api unblocks.
        let pick = reconcile_and_pick(&mut r, &|sid| {
            (sid == "s1").then_some(SessionStatus::Completed)
        });
        assert_eq!(pick, vec!["api"]);
        assert_eq!(
            r.workstream("schema").unwrap().status,
            WorkstreamStatus::Complete
        );
        assert_eq!(
            r.workstream("ui").unwrap().status,
            WorkstreamStatus::Blocked
        );
    }

    #[test]
    fn reconcile_leaves_running_workstream_alone() {
        let mut r = ranch(vec![ws(
            "schema",
            &[],
            WorkstreamStatus::Running,
            Some("s1"),
        )]);
        // Session still running → no change, nothing new to start.
        let pick = reconcile_and_pick(&mut r, &|_| Some(SessionStatus::Running));
        assert!(pick.is_empty());
        assert_eq!(
            r.workstream("schema").unwrap().status,
            WorkstreamStatus::Running
        );
    }

    #[test]
    fn compose_task_includes_goal_and_rules() {
        let r = ranch(vec![]);
        let mut w = ws("api", &["schema"], WorkstreamStatus::Ready, None);
        w.acceptance = vec!["tests pass".into()];
        let task = compose_task(&r, &w);
        assert!(task.contains("Your workstream: API (api)"));
        assert!(task.contains("Depends on (complete): schema"));
        assert!(task.contains("tests pass"));
        assert!(task.contains("Coordination rules"));
    }
}
