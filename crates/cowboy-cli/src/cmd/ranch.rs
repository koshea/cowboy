//! `cowboy ranch` — create and inspect Ranch Plans (multi-workstream tasks).
//!
//! The plan lives at `.cowboy/ranches/<id>/ranch.yaml` and is committed (the
//! shared source of truth). `create` writes a skeleton to fill in; launching
//! workstreams arrives in a later stage.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, LeaseMode, SessionStatus};
use cowboy_core::ranch::{self, Ranch, RanchStatus, Workstream, WorkstreamStatus};
use cowboy_core::scope::{self, ProposalStatus, ScopeChange, ScopeProposal};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use tokio::runtime::Handle;

use crate::cli::RanchCommand;
use crate::cmd::daemon;

pub async fn run(command: RanchCommand) -> Result<()> {
    let root = crate::cmd::project_root()?;
    match command {
        RanchCommand::Create { title, goal } => create(&root, &title, goal),
        RanchCommand::Add {
            id,
            workstream,
            goal,
            title,
            depends_on,
            acceptance,
            expects,
        } => add_workstream(
            &root,
            &id,
            &workstream,
            &goal,
            title,
            depends_on,
            acceptance,
            expects,
        ),
        RanchCommand::Plan { goal } => plan(goal).await,
        RanchCommand::Status { id } => status(&root, id),
        RanchCommand::Start { id } => start(&root, &id).await,
        RanchCommand::Attach { id, workstream } => attach(&root, &id, &workstream).await,
        RanchCommand::Complete { id, workstream } => complete(&root, &id, &workstream),
        RanchCommand::Accept { id, workstream } => accept(&root, &id, &workstream),
        RanchCommand::Watch { id } => watch(&root, &id).await,
        RanchCommand::Propose {
            id,
            summary,
            rationale,
            add_workstream,
            remove_workstream,
            note,
            title,
            goal,
            depends_on,
        } => propose(
            &root,
            &id,
            summary,
            rationale,
            add_workstream,
            remove_workstream,
            note,
            title,
            goal,
            depends_on,
        ),
        RanchCommand::Proposals { id, all } => list_proposals(&root, &id, all),
        RanchCommand::Approve { id, proposal } => approve(&root, &id, &proposal),
        RanchCommand::Reject {
            id,
            proposal,
            reason,
        } => reject(&root, &id, &proposal, reason),
    }
}

/// `cowboy ranch attach <id> <workstream>` — attach to that workstream's session.
async fn attach(root: &std::path::Path, id: &str, workstream: &str) -> Result<()> {
    let ranch = ranch::load(root, id)?;
    let ws = ranch
        .workstream(workstream)
        .with_context(|| format!("no workstream `{workstream}` in ranch `{id}`"))?;
    let sid = ws
        .session_id
        .clone()
        .with_context(|| format!("{workstream} has not been started yet"))?;
    crate::cmd::attach::run(sid).await
}

/// `cowboy ranch complete <id> <workstream>` — manually mark a workstream done
/// (e.g. after verifying acceptance), promote its artifacts, and unblock
/// dependents. Useful when the session ended without a clean completion signal.
fn complete(root: &std::path::Path, id: &str, workstream: &str) -> Result<()> {
    mark_done(root, id, workstream, "marked complete")
}

/// `cowboy ranch accept <id> <workstream>` — sign off on a workstream that
/// finished but is held at the acceptance gate (`WaitingForUser`): verifies the
/// human criteria are met, marks it complete, promotes its artifacts, and
/// unblocks dependents. Functionally identical to `complete`, named for the gate.
fn accept(root: &std::path::Path, id: &str, workstream: &str) -> Result<()> {
    mark_done(root, id, workstream, "accepted")
}

/// Shared body for `complete`/`accept`: force a workstream to Complete, promote
/// its outputs, recompute readiness, and report newly-unblocked dependents.
fn mark_done(root: &std::path::Path, id: &str, workstream: &str, verb: &str) -> Result<()> {
    let mut ranch = ranch::load(root, id)?;
    {
        let ws = ranch
            .workstream_mut(workstream)
            .with_context(|| format!("no workstream `{workstream}` in ranch `{id}`"))?;
        ws.status = WorkstreamStatus::Complete;
    }
    let ws = ranch.workstream(workstream).unwrap().clone();
    let n = promote_artifacts(root, &ranch, &ws);
    let newly = ranch.recompute_readiness();
    if !ranch.workstreams.is_empty() && ranch.workstreams.iter().all(|w| w.status.is_done()) {
        ranch.status = RanchStatus::Complete;
    }
    ranch.updated_ms = now_ms();
    ranch::save(root, &ranch)?;
    println!("✓ {workstream} {verb} — promoted {n} artifact(s)");
    if !newly.is_empty() {
        println!("newly ready: {}", newly.join(", "));
        println!("launch them with `cowboy ranch start {id}`.");
    }
    if ranch.status == RanchStatus::Complete {
        println!("ranch complete 🎉");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scope-change proposals (gated edits to the plan)
// ---------------------------------------------------------------------------

/// `cowboy ranch propose` — record a pending scope-change proposal. The plan is
/// NOT modified until a human approves it.
#[allow(clippy::too_many_arguments)]
fn propose(
    root: &std::path::Path,
    id: &str,
    summary: String,
    rationale: Option<String>,
    add_workstream: Option<String>,
    remove_workstream: Option<String>,
    note: bool,
    title: Option<String>,
    goal: Option<String>,
    depends_on: Vec<String>,
) -> Result<()> {
    // The ranch must exist (and gives a clear error otherwise).
    let ranch = ranch::load(root, id)?;
    let change = match (add_workstream, remove_workstream, note) {
        (Some(ws_id), None, false) => {
            if ranch.workstream(&ws_id).is_some() {
                bail!("workstream `{ws_id}` already exists in ranch `{id}`");
            }
            ScopeChange::AddWorkstream {
                workstream: Workstream {
                    title: title.unwrap_or_else(|| ws_id.clone()),
                    goal: goal.unwrap_or_default(),
                    depends_on,
                    id: ws_id,
                    status: WorkstreamStatus::Planned,
                    session_id: None,
                    branch: None,
                    worktree_path: None,
                    expected_artifacts: vec![],
                    acceptance: vec![],
                },
            }
        }
        (None, Some(ws_id), false) => {
            if ranch.workstream(&ws_id).is_none() {
                bail!("no workstream `{ws_id}` in ranch `{id}`");
            }
            ScopeChange::RemoveWorkstream { id: ws_id }
        }
        (None, None, true) => ScopeChange::Note,
        _ => bail!("specify exactly one of --add-workstream, --remove-workstream, or --note"),
    };
    let p = ScopeProposal {
        id: scope::fresh_id(root, id),
        ranch_id: id.to_string(),
        from: "user".to_string(),
        summary,
        rationale,
        change,
        status: ProposalStatus::Pending,
        created_ms: now_ms(),
        decided_ms: None,
        decision_reason: None,
    };
    scope::save(root, &p)?;
    println!("✓ filed proposal {} — {}", p.id, p.change.label());
    println!("  review with `cowboy ranch proposals {id}`,");
    println!("  then `cowboy ranch approve {id} {}` (or reject).", p.id);
    Ok(())
}

/// `cowboy ranch proposals <id>` — list scope-change proposals.
fn list_proposals(root: &std::path::Path, id: &str, all: bool) -> Result<()> {
    ranch::load(root, id)?; // validate
    let proposals = scope::list(root, id);
    let shown: Vec<_> = proposals
        .iter()
        .filter(|p| all || p.status == ProposalStatus::Pending)
        .collect();
    if shown.is_empty() {
        println!(
            "no {}proposals for ranch `{id}`",
            if all { "" } else { "pending " }
        );
        return Ok(());
    }
    println!("{:<8} {:<9} {:<8} CHANGE / SUMMARY", "ID", "STATUS", "FROM");
    for p in shown {
        println!(
            "{:<8} {:<9} {:<8} {} — {}",
            p.id,
            proposal_status_str(p.status),
            p.from,
            p.change.label(),
            p.summary
        );
        if let Some(r) = &p.rationale {
            println!("         rationale: {r}");
        }
    }
    Ok(())
}

/// `cowboy ranch approve <id> <proposal>` — apply a pending proposal's change to
/// the plan and mark it approved.
fn approve(root: &std::path::Path, id: &str, pid: &str) -> Result<()> {
    let mut p = scope::load(root, id, pid)?;
    if p.status != ProposalStatus::Pending {
        bail!(
            "proposal {pid} is already {}",
            proposal_status_str(p.status)
        );
    }
    let mut ranch = ranch::load(root, id)?;
    let msg = apply_change(&mut ranch, &p.change)?;
    ranch.updated_ms = now_ms();
    ranch::save(root, &ranch)?;
    p.status = ProposalStatus::Approved;
    p.decided_ms = Some(now_ms());
    scope::save(root, &p)?;
    println!("✓ approved {pid}: {msg}");
    Ok(())
}

/// `cowboy ranch reject <id> <proposal>` — record a rejection; plan unchanged.
fn reject(root: &std::path::Path, id: &str, pid: &str, reason: Option<String>) -> Result<()> {
    let mut p = scope::load(root, id, pid)?;
    if p.status != ProposalStatus::Pending {
        bail!(
            "proposal {pid} is already {}",
            proposal_status_str(p.status)
        );
    }
    p.status = ProposalStatus::Rejected;
    p.decided_ms = Some(now_ms());
    p.decision_reason = reason;
    scope::save(root, &p)?;
    println!("✓ rejected {pid} (plan unchanged)");
    Ok(())
}

/// Apply a scope change to a ranch in memory, returning a human-readable summary.
/// Rejects unsafe edits (duplicate add, removing started/done work).
fn apply_change(ranch: &mut Ranch, change: &ScopeChange) -> Result<String> {
    match change {
        ScopeChange::AddWorkstream { workstream } => {
            if ranch.workstream(&workstream.id).is_some() {
                bail!("workstream `{}` already exists", workstream.id);
            }
            let mut w = workstream.clone();
            w.status = WorkstreamStatus::Planned; // a freshly-added workstream starts planned
            let wid = w.id.clone();
            ranch.workstreams.push(w);
            ranch.recompute_readiness();
            Ok(format!("added workstream `{wid}`"))
        }
        ScopeChange::RemoveWorkstream { id } => {
            let Some(w) = ranch.workstream(id) else {
                bail!("no workstream `{id}`");
            };
            // Don't rip out work that's already running or done.
            if !matches!(
                w.status,
                WorkstreamStatus::Planned | WorkstreamStatus::Blocked | WorkstreamStatus::Ready
            ) {
                bail!(
                    "can't remove `{id}`: it is {} (only not-yet-started workstreams can be removed)",
                    ws_status(w.status)
                );
            }
            // Refuse if another workstream still depends on it.
            if let Some(dep) = ranch
                .workstreams
                .iter()
                .find(|o| o.depends_on.iter().any(|d| d == id))
            {
                bail!("can't remove `{id}`: `{}` depends on it", dep.id);
            }
            ranch.workstreams.retain(|o| &o.id != id);
            ranch.recompute_readiness();
            Ok(format!("removed workstream `{id}`"))
        }
        ScopeChange::Note => Ok("note acknowledged (no plan change)".to_string()),
    }
}

fn proposal_status_str(s: ProposalStatus) -> &'static str {
    match s {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Approved => "approved",
        ProposalStatus::Rejected => "rejected",
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
         auto_advance: true  # daemon launches ready workstreams as deps finish\n\
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
    println!("  add workstreams: `cowboy ranch add {id} <ws-id> --goal \"…\" [--depends-on a,b]`");
    println!("  then check it with `cowboy ranch status {id}`.");
    Ok(())
}

/// `cowboy ranch plan "<goal>"` — let the agent decompose a goal into a ranch.
/// Starts an interactive session seeded to research read-only and call the
/// `propose_ranch` tool, which writes a draft ranch.yaml for the user to review.
async fn plan(goal: String) -> Result<()> {
    let task = format!(
        "Plan a multi-workstream Ranch Plan for the goal below — DO NOT implement anything or \
         edit files. Research the codebase READ-ONLY (read/grep/ls) to find the natural seams, \
         then decompose the goal into independent, parallelizable workstreams wired by \
         dependencies (a DAG), and call the `propose_ranch` tool ONCE with the full decomposition \
         (ids, goals, depends_on, expected artifacts, acceptance criteria). Keep workstreams \
         coarse-grained. After it's drafted, summarize the plan and tell me to review it with \
         `cowboy ranch status`.\n\nGoal: {goal}"
    );
    let flags = crate::cli::StartFlags {
        attach_if_active: false,
        read_only: false,
        new_worktree: false,
        force: false,
    };
    crate::cmd::session::run(Some(task), flags, None).await
}

/// Add a workstream to an existing ranch, validating the dependency graph
/// (rejects a cycle, an unknown `depends_on`, or a duplicate id) before saving —
/// so building a ranch never silently produces an unrunnable plan.
#[allow(clippy::too_many_arguments)]
fn add_workstream(
    root: &std::path::Path,
    ranch_id: &str,
    ws_id: &str,
    goal: &str,
    title: Option<String>,
    depends_on: Vec<String>,
    acceptance: Vec<String>,
    expects: Vec<String>,
) -> Result<()> {
    let mut ranch = ranch::load(root, ranch_id)?;
    if ranch.workstreams.iter().any(|w| w.id == ws_id) {
        bail!("workstream `{ws_id}` already exists in ranch `{ranch_id}`");
    }
    ranch.workstreams.push(Workstream {
        id: ws_id.to_string(),
        title: title.unwrap_or_else(|| ws_id.to_string()),
        goal: goal.to_string(),
        depends_on,
        status: WorkstreamStatus::Planned,
        session_id: None,
        branch: None,
        worktree_path: None,
        expected_artifacts: expects,
        acceptance,
    });
    // Reuse the graph validator: a typo'd dependency or a cycle is caught here,
    // not at `ranch start` (where it would silently deadlock).
    ranch
        .validate()
        .map_err(|e| anyhow::anyhow!("adding `{ws_id}` would break the plan: {e}"))?;
    ranch.updated_ms = now_ms();
    ranch::save(root, &ranch)?;

    let ws = ranch.workstream(ws_id).expect("just added");
    let deps = if ws.depends_on.is_empty() {
        "none (ready to start)".to_string()
    } else {
        ws.depends_on.join(", ")
    };
    println!("✓ added workstream `{ws_id}` to ranch `{ranch_id}` (depends on: {deps})");
    println!(
        "  next: `cowboy ranch status {ranch_id}` · start with `cowboy ranch start {ranch_id}`"
    );
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
    // Dependency tree: lay workstreams out by dependency depth (deepest chain),
    // indented so the execution order and what-waits-on-what read at a glance.
    println!("\n  ✓ done · ⟳ running · ◷ ready · ⊘ blocked · ⏸ waiting · · planned");
    println!("\nworkstreams (top runs first):");
    let depths = dep_depths(&r);
    let mut order: Vec<&Workstream> = r.workstreams.iter().collect();
    order.sort_by(|a, b| {
        let (da, db) = (depths.get(&a.id).copied(), depths.get(&b.id).copied());
        da.cmp(&db).then_with(|| a.id.cmp(&b.id))
    });
    for w in order {
        let indent = "  ".repeat(depths.get(&w.id).copied().unwrap_or(0) + 1);
        let sess = w
            .session_id
            .as_deref()
            .map(|s| format!(" · {s}"))
            .unwrap_or_default();
        let after = if w.depends_on.is_empty() {
            String::new()
        } else {
            format!("   ← {}", w.depends_on.join(", "))
        };
        println!(
            "{indent}{} {} — {}{sess}{after}",
            ws_glyph(w.status),
            w.id,
            ws_status(w.status)
        );
    }
    let ready: Vec<_> = r.ready_workstreams().iter().map(|w| w.id.clone()).collect();
    if !ready.is_empty() {
        println!("\nready to start: {}", ready.join(", "));
        println!("launch with `cowboy ranch start {}`", r.id);
    }
    Ok(())
}

/// A glyph per workstream status, matching the dashboard's vocabulary.
fn ws_glyph(s: WorkstreamStatus) -> &'static str {
    match s {
        WorkstreamStatus::Complete | WorkstreamStatus::Integrated => "✓",
        WorkstreamStatus::MergeReady => "⇧",
        WorkstreamStatus::Running | WorkstreamStatus::Starting => "⟳",
        WorkstreamStatus::Ready => "◷",
        WorkstreamStatus::WaitingForUser => "⏸",
        WorkstreamStatus::Blocked => "⊘",
        WorkstreamStatus::Failed | WorkstreamStatus::Cancelled => "✗",
        WorkstreamStatus::Planned => "·",
    }
}

/// Dependency depth of each workstream = the longest chain of `depends_on`
/// beneath it (0 for a root). Used to lay the tree out in execution order.
/// Assumes a DAG (the caller validates first); a stray cycle just yields 0s.
fn dep_depths(r: &Ranch) -> HashMap<String, usize> {
    let by_id: HashMap<&str, &Workstream> =
        r.workstreams.iter().map(|w| (w.id.as_str(), w)).collect();
    fn depth(
        id: &str,
        by_id: &HashMap<&str, &Workstream>,
        memo: &mut HashMap<String, usize>,
    ) -> usize {
        if let Some(&v) = memo.get(id) {
            return v;
        }
        memo.insert(id.to_string(), 0); // cycle guard (DAG expected)
        let v = by_id
            .get(id)
            .map(|w| {
                w.depends_on
                    .iter()
                    .map(|d| 1 + depth(d, by_id, memo))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        memo.insert(id.to_string(), v);
        v
    }
    let mut memo = HashMap::new();
    for w in &r.workstreams {
        depth(&w.id, &by_id, &mut memo);
    }
    memo
}

/// `cowboy ranch start <id>` — reconcile finished workstreams, then launch every
/// newly-ready one in its own worktree/branch. Idempotent + re-entrant: run it
/// again as workstreams complete to advance the dependency graph.
async fn start(root: &std::path::Path, id: &str) -> Result<()> {
    daemon::ensure_running().await?;
    for line in advance(root, id).await? {
        println!("{line}");
    }
    Ok(())
}

/// Reconcile finished workstreams, promote their outputs, launch newly-ready
/// ones, persist the ranch, and return human-readable log lines describing what
/// happened. Shared by `start` (prints them) and the `watch` dashboard (renders
/// them in-pane, so it never writes to the raw-mode terminal). Assumes the
/// daemon is already running.
async fn advance(root: &std::path::Path, id: &str) -> Result<Vec<String>> {
    let mut log: Vec<String> = Vec::new();
    let mut ranch = ranch::load(root, id)?;
    // Reject a broken dependency graph up front: a cycle or a typo'd `depends_on`
    // would otherwise silently leave workstreams blocked forever with no error.
    ranch
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid ranch {id}: {e}"))?;

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
    let reconciled = reconcile_and_pick(&mut ranch, &|sid| session_status.get(sid).copied());

    // Workstreams whose session finished: promote their outputs for review, but
    // they do NOT unblock dependents until the user signs off. Each is an
    // interactive session you attach to, refine, and `/accept` when happy.
    for ws_id in &reconciled.awaiting_acceptance {
        if let Some(ws) = ranch.workstream(ws_id).cloned() {
            let n = promote_artifacts(root, &ranch, &ws);
            log.push(format!(
                "{ws_id} finished a first attempt — promoted {n} artifact(s) for review"
            ));
            log.push(format!(
                "  attach with `cowboy ranch attach {} {ws_id}`, then `/accept` in-session \
                 (or `cowboy ranch accept {} {ws_id}`) to sign off and unblock dependents",
                ranch.id, ranch.id
            ));
        }
    }

    let mut started: Vec<(String, String, String)> = Vec::new();
    for ws_id in &reconciled.ready {
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
                log.push(format!("skip {}: worktree: {message}", ws.id));
                continue;
            }
            other => bail!("unexpected daemon response: {other:?}"),
        };
        let task = compose_task(root, &ranch, &ws);
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
            DaemonResp::LeaseDenied { .. } => {
                log.push(format!("skip {}: worktree already in use", ws.id))
            }
            DaemonResp::Err { message } => log.push(format!("skip {}: {message}", ws.id)),
            other => bail!("unexpected daemon response: {other:?}"),
        }
    }

    // Reflect overall ranch state.
    let any_active = ranch.workstreams.iter().any(|w| {
        matches!(
            w.status,
            WorkstreamStatus::Running | WorkstreamStatus::Starting
        )
    });
    let any_awaiting = ranch
        .workstreams
        .iter()
        .any(|w| w.status == WorkstreamStatus::WaitingForUser);
    if !ranch.workstreams.is_empty() && ranch.workstreams.iter().all(|w| w.status.is_done()) {
        ranch.status = RanchStatus::Complete;
    } else if any_active {
        ranch.status = RanchStatus::Running;
    } else if any_awaiting {
        // Nothing running and something needs sign-off → pause for the user.
        ranch.status = RanchStatus::WaitingForUser;
    }
    ranch.updated_ms = now_ms();
    ranch::save(root, &ranch)?;

    if started.is_empty() {
        log.push("nothing ready to start.".into());
    } else {
        log.push(format!("started {} workstream(s):", started.len()));
        for (wid, sid, branch) in &started {
            log.push(format!("  {wid}  → session {sid}  on {branch}"));
        }
    }
    let awaiting: Vec<_> = ranch
        .workstreams
        .iter()
        .filter(|w| w.status == WorkstreamStatus::WaitingForUser)
        .map(|w| w.id.clone())
        .collect();
    if !awaiting.is_empty() {
        log.push(format!("awaiting sign-off: {}", awaiting.join(", ")));
    }
    let blocked: Vec<_> = ranch
        .workstreams
        .iter()
        .filter(|w| w.status == WorkstreamStatus::Blocked)
        .map(|w| w.id.clone())
        .collect();
    if !blocked.is_empty() {
        log.push(format!("still blocked: {}", blocked.join(", ")));
        log.push(format!(
            "re-run `cowboy ranch start {}` as workstreams complete.",
            ranch.id
        ));
    }
    if ranch.status == RanchStatus::Complete {
        log.push("ranch complete 🎉".into());
    }
    Ok(log)
}

/// `cowboy ranch watch <id>` — a live TUI dashboard for a ranch: the workstream
/// table refreshes on a 1s poll, `s` advances the plan (reconcile + launch ready)
/// in-pane, `r` refreshes, `q`/Esc quits. Advance output is rendered into the log
/// pane rather than printed, so it never corrupts the raw-mode terminal.
async fn watch(root: &Path, id: &str) -> Result<()> {
    daemon::ensure_running().await?;
    // Validate up-front so a bad id errors cleanly before we enter raw mode.
    ranch::load(root, id)?;
    let handle = Handle::current();
    let root = root.to_path_buf();
    let id = id.to_string();
    // The render loop is synchronous (crossterm blocking poll); daemon calls hop
    // back onto the runtime via the captured handle.
    tokio::task::spawn_blocking(move || dashboard_loop(&handle, &root, &id))
        .await
        .context("dashboard task panicked")?
}

/// A non-saving display snapshot: load the ranch, query live session statuses,
/// and reconcile in memory (no write) so the table reflects the dependency graph.
async fn live_view(root: &Path, id: &str) -> Result<(Ranch, HashMap<String, SessionStatus>)> {
    let mut ranch = ranch::load(root, id)?;
    let mut session_status: HashMap<String, SessionStatus> = HashMap::new();
    for w in &ranch.workstreams {
        if let Some(sid) = &w.session_id {
            if let Ok(DaemonResp::Session { info }) =
                daemon::request(DaemonReq::GetSession { id: sid.clone() }).await
            {
                session_status.insert(sid.clone(), info.status);
            }
        }
    }
    // Reflect readiness/finished transitions for display only (result discarded).
    reconcile_and_pick(&mut ranch, &|sid| session_status.get(sid).copied());
    Ok((ranch, session_status))
}

type DashTerm = Terminal<CrosstermBackend<Stdout>>;

fn dashboard_loop(handle: &Handle, root: &Path, id: &str) -> Result<()> {
    let mut terminal = setup_dashboard_terminal()?;
    let mut log: Vec<String> = Vec::new();
    let mut view = handle.block_on(live_view(root, id))?;
    let res = (|| -> Result<()> {
        loop {
            terminal.draw(|f| draw_dashboard(f, &view.0, &view.1, &log))?;
            // Poll with a 1s timeout → auto-refresh when idle.
            if event::poll(Duration::from_secs(1))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('s') => {
                            match handle.block_on(advance(root, id)) {
                                Ok(lines) => log.extend(lines),
                                Err(e) => log.push(format!("error: {e}")),
                            }
                            if let Ok(v) = handle.block_on(live_view(root, id)) {
                                view = v;
                            }
                        }
                        KeyCode::Char('r') => {
                            if let Ok(v) = handle.block_on(live_view(root, id)) {
                                view = v;
                            }
                        }
                        _ => {}
                    }
                }
            } else if let Ok(v) = handle.block_on(live_view(root, id)) {
                view = v;
            }
        }
        Ok(())
    })();
    restore_dashboard_terminal(&mut terminal)?;
    res
}

fn setup_dashboard_terminal() -> Result<DashTerm> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_dashboard_terminal(terminal: &mut DashTerm) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw_dashboard(
    f: &mut Frame,
    ranch: &Ranch,
    session_status: &HashMap<String, SessionStatus>,
    log: &[String],
) {
    let log_h = if log.is_empty() { 0 } else { 8 };
    let chunks = Layout::vertical([
        Constraint::Length(4),     // header
        Constraint::Min(3),        // workstream table
        Constraint::Length(log_h), // advance log (hidden when empty)
        Constraint::Length(1),     // footer / key hints
    ])
    .split(f.area());

    // Header: title, status, goal.
    let mut header = vec![
        Line::from(vec![
            Span::styled(
                format!("ranch {} ", ranch.id),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("— {}", ranch.title)),
        ]),
        Line::from(vec![
            Span::raw("status: "),
            Span::styled(ranch_status(ranch.status), ranch_status_style(ranch.status)),
        ]),
    ];
    if !ranch.goal.is_empty() {
        header.push(Line::from(format!("goal: {}", ranch.goal)));
    }
    f.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::ALL)),
        chunks[0],
    );

    // Workstream table.
    let header_row = Row::new(["WORKSTREAM", "STATUS", "SESSION", "DEPENDS ON"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = ranch.workstreams.iter().map(|w| {
        let sess = w.session_id.as_deref().unwrap_or("-");
        // Show live session status alongside the workstream status when it adds info.
        let sess_cell = match w.session_id.as_deref().and_then(|s| session_status.get(s)) {
            Some(st) => format!("{sess} ({})", session_status_str(*st)),
            None => sess.to_string(),
        };
        Row::new(vec![
            Cell::from(w.id.clone()),
            Cell::from(Span::styled(ws_status(w.status), ws_status_style(w.status))),
            Cell::from(sess_cell),
            Cell::from(w.depends_on.join(", ")),
        ])
    });
    let widths = [
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(28),
        Constraint::Min(10),
    ];
    f.render_widget(
        Table::new(rows, widths).header(header_row).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" workstreams "),
        ),
        chunks[1],
    );

    // Advance log pane (only when there's output).
    if log_h > 0 {
        let tail: Vec<Line> = log
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|l| Line::from(l.clone()))
            .collect();
        f.render_widget(
            Paragraph::new(tail).block(Block::default().borders(Borders::ALL).title(" log ")),
            chunks[2],
        );
    }

    // Footer key hints.
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " q quit · s advance (launch ready) · r refresh · auto-refresh 1s ",
            Style::default().fg(Color::DarkGray),
        )])),
        chunks[3],
    );
}

fn ranch_status_style(s: RanchStatus) -> Style {
    let c = match s {
        RanchStatus::Complete => Color::Green,
        RanchStatus::Running | RanchStatus::Integrating => Color::Cyan,
        RanchStatus::WaitingForUser | RanchStatus::Paused => Color::Yellow,
        RanchStatus::Failed | RanchStatus::Cancelled => Color::Red,
        _ => Color::Gray,
    };
    Style::default().fg(c)
}

fn ws_status_style(s: WorkstreamStatus) -> Style {
    let c = match s {
        WorkstreamStatus::Complete | WorkstreamStatus::Integrated => Color::Green,
        WorkstreamStatus::Running | WorkstreamStatus::Starting => Color::Cyan,
        WorkstreamStatus::Ready | WorkstreamStatus::MergeReady => Color::LightGreen,
        WorkstreamStatus::WaitingForUser => Color::Yellow,
        WorkstreamStatus::Blocked => Color::DarkGray,
        WorkstreamStatus::Failed | WorkstreamStatus::Cancelled => Color::Red,
        WorkstreamStatus::Planned => Color::Gray,
    };
    Style::default().fg(c)
}

fn session_status_str(s: SessionStatus) -> &'static str {
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

/// What `reconcile_and_pick` decided this run.
struct Reconciled {
    /// Ids ready to launch now (deps complete).
    ready: Vec<String>,
    /// Ids whose session finished without an explicit sign-off: they're now
    /// `WaitingForUser`. Their artifacts are promoted for review, but dependents
    /// stay blocked until the user signs off (`/accept` in-session, or the
    /// `cowboy ranch accept` CLI fallback). A workstream is never auto-completed.
    awaiting_acceptance: Vec<String>,
}

/// Reconcile already-started workstreams from their session status, recompute
/// readiness, and report what's ready and what is awaiting acceptance sign-off.
/// Pure (status lookup injected), so it's unit-testable without a daemon.
fn reconcile_and_pick(
    ranch: &mut Ranch,
    session_status: &dyn Fn(&str) -> Option<SessionStatus>,
) -> Reconciled {
    let mut awaiting_acceptance = Vec::new();
    for w in &mut ranch.workstreams {
        if matches!(
            w.status,
            WorkstreamStatus::Running | WorkstreamStatus::Starting
        ) {
            if let Some(sid) = &w.session_id {
                match session_status(sid) {
                    // A workstream is interactive: it's never auto-completed. A
                    // session that ended without an explicit sign-off (the user
                    // quit, or it crashed clean) parks here for the user to attach,
                    // review, and `/accept` — dependents stay blocked until then.
                    // Explicit sign-off (`/accept` / `ranch accept`) sets `Complete`
                    // directly via `mark_done`, never through this path.
                    Some(SessionStatus::Completed) => {
                        w.status = WorkstreamStatus::WaitingForUser;
                        awaiting_acceptance.push(w.id.clone());
                    }
                    Some(SessionStatus::Failed) | Some(SessionStatus::Stale) => {
                        w.status = WorkstreamStatus::Failed
                    }
                    _ => {}
                }
            }
        }
    }
    ranch.recompute_readiness();
    let ready = ranch
        .workstreams
        .iter()
        .filter(|w| w.status == WorkstreamStatus::Ready)
        .map(|w| w.id.clone())
        .collect();
    Reconciled {
        ready,
        awaiting_acceptance,
    }
}

/// Promote a completed workstream's published artifacts (+ handoff) from its
/// session dir in its worktree into the ranch's committed artifact store, so
/// downstream workstreams (and reviewers) can consume them. Returns the count.
fn promote_artifacts(
    root: &std::path::Path,
    ranch: &Ranch,
    ws: &cowboy_core::ranch::Workstream,
) -> usize {
    let (Some(wt), Some(sid)) = (&ws.worktree_path, &ws.session_id) else {
        return 0;
    };
    let session_dir = crate::session::session_dir(wt, sid);
    let dest = ranch::ranch_artifact_dir(root, &ranch.id, &ws.id);
    if std::fs::create_dir_all(&dest).is_err() {
        return 0;
    }
    let mut n = 0;
    for a in cowboy_core::artifact::list_in(&session_dir) {
        let src = session_dir.join(&a.path);
        if let Some(name) = a.path.file_name() {
            if std::fs::copy(&src, dest.join(name)).is_ok() {
                n += 1;
            }
        }
    }
    // The handoff is the headline output; promote it too if present.
    let handoff = session_dir.join("handoff.md");
    if handoff.exists() {
        let _ = std::fs::copy(&handoff, dest.join("handoff.md"));
    }
    n
}

/// Build the worker task prompt for a workstream, injecting the promoted
/// artifacts of its completed dependencies so it can consume them directly.
fn compose_task(
    root: &std::path::Path,
    ranch: &Ranch,
    ws: &cowboy_core::ranch::Workstream,
) -> String {
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

    // Inline the dependencies' promoted artifacts (capped) so the worker has the
    // upstream contracts/handoffs in context.
    let mut deps_block = String::new();
    for dep in &ws.depends_on {
        let dir = ranch::ranch_artifact_dir(root, &ranch.id, dep);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned());
            let Some(name) = name else { continue };
            if let Ok(body) = std::fs::read_to_string(&f) {
                let body = truncate(&body, 8000);
                deps_block.push_str(&format!("\n### {dep}/{name}\n{body}\n"));
            }
        }
    }
    if !deps_block.is_empty() {
        s.push_str("\nArtifacts from your dependencies (consume these):\n");
        s.push_str(&deps_block);
    }

    s.push_str(
        "\nCoordination rules:\n\
         - Work only on this workstream, in this worktree.\n\
         - Publish status/blockers/outputs with your tools (artifact / blocked / handoff).\n\
         - Do NOT edit the ranch plan. If it looks wrong (a workstream is missing, \
           unnecessary, or misscoped), use `propose_scope_change` to file a proposal \
           for the user to approve — never change scope on your own.\n\
         - Make a solid first attempt now: publish the expected artifacts and a handoff, \
           then stop. This is an interactive workstream — the user will attach, review \
           your work, refine it with you, and sign off with `/accept` when satisfied. \
           Do NOT consider the workstream complete yourself; wait for their direction.\n",
    );
    s
}

/// Truncate `s` to at most `max` bytes (on a char boundary), noting the cut.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(truncated)", &s[..end])
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

use cowboy_core::time::now_ms;

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::ranch::Workstream;
    use ratatui::backend::TestBackend;

    #[test]
    fn draw_dashboard_renders_header_table_and_keys() {
        let r = ranch(vec![
            ws("schema", &[], WorkstreamStatus::Complete, Some("s1")),
            ws("api", &["schema"], WorkstreamStatus::Running, Some("s2")),
            ws("ui", &["api"], WorkstreamStatus::Blocked, None),
        ]);
        let mut statuses = HashMap::new();
        statuses.insert("s2".to_string(), SessionStatus::Running);
        let log = vec!["api → session s2 on cowboy/r-api".to_string()];

        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_dashboard(f, &r, &statuses, &log))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();

        assert!(text.contains("ranch r"));
        assert!(text.contains("WORKSTREAM"));
        assert!(text.contains("schema"));
        assert!(text.contains("blocked")); // ui's status
        assert!(text.contains("s2 (running)")); // api session + live session status
        assert!(text.contains("q quit"));
        assert!(text.contains("cowboy/r-api")); // the advance log line
    }

    #[test]
    fn draw_dashboard_hides_empty_log_pane() {
        // With no log lines the dashboard still renders (log pane collapses to 0).
        let r = ranch(vec![ws("only", &[], WorkstreamStatus::Planned, None)]);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_dashboard(f, &r, &HashMap::new(), &[]))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("only"));
        assert!(!text.contains(" log ")); // no log pane title when empty
    }

    #[test]
    fn dep_depths_layer_the_graph_in_execution_order() {
        let r = ranch(vec![
            ws("schema", &[], WorkstreamStatus::Planned, None),
            ws("api", &["schema"], WorkstreamStatus::Planned, None),
            ws("ui", &["api"], WorkstreamStatus::Planned, None),
            ws(
                "integration",
                &["api", "ui"],
                WorkstreamStatus::Planned,
                None,
            ),
        ]);
        let d = dep_depths(&r);
        assert_eq!(d["schema"], 0);
        assert_eq!(d["api"], 1);
        assert_eq!(d["ui"], 2);
        assert_eq!(d["integration"], 3); // max(api=1, ui=2) + 1
    }

    #[test]
    fn add_workstream_appends_and_validates_the_graph() {
        let dir = std::env::temp_dir().join(format!("cowboy-ranch-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ranch::save(
            &dir,
            &ranch(vec![ws("schema", &[], WorkstreamStatus::Planned, None)]),
        )
        .unwrap();

        // Add a dependent workstream from the CLI path.
        add_workstream(
            &dir,
            "r",
            "api",
            "build the api",
            None,
            vec!["schema".into()],
            vec![],
            vec![],
        )
        .unwrap();
        let r = ranch::load(&dir, "r").unwrap();
        let api = r.workstream("api").expect("api was added");
        assert_eq!(api.depends_on, vec!["schema".to_string()]);
        assert_eq!(api.goal, "build the api");
        assert_eq!(api.title, "api"); // defaults to the id

        // A duplicate id is rejected.
        assert!(
            add_workstream(&dir, "r", "api", "dup", None, vec![], vec![], vec![]).is_err(),
            "duplicate workstream id must be rejected"
        );
        // An unknown dependency is rejected by graph validation (not silently saved).
        assert!(
            add_workstream(
                &dir,
                "r",
                "ui",
                "ui",
                None,
                vec!["nope".into()],
                vec![],
                vec![]
            )
            .is_err(),
            "unknown depends_on must be rejected"
        );
        // The bad adds didn't persist.
        let r = ranch::load(&dir, "r").unwrap();
        assert_eq!(r.workstreams.len(), 2, "only schema + api persisted");

        let _ = std::fs::remove_dir_all(&dir);
    }

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
            auto_advance: true,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn reconcile_holds_finished_workstream_for_signoff() {
        // schema is Running on session s1; api waits on schema; ui waits on api.
        let mut r = ranch(vec![
            ws("schema", &[], WorkstreamStatus::Running, Some("s1")),
            ws("api", &["schema"], WorkstreamStatus::Planned, None),
            ws("ui", &["api"], WorkstreamStatus::Planned, None),
        ]);
        // s1 has Completed → a workstream is never auto-completed, so schema parks
        // at WaitingForUser and api stays blocked until the user signs off.
        let rec = reconcile_and_pick(&mut r, &|sid| {
            (sid == "s1").then_some(SessionStatus::Completed)
        });
        assert!(rec.ready.is_empty());
        assert_eq!(rec.awaiting_acceptance, vec!["schema"]);
        assert_eq!(
            r.workstream("schema").unwrap().status,
            WorkstreamStatus::WaitingForUser
        );
        assert_eq!(
            r.workstream("api").unwrap().status,
            WorkstreamStatus::Blocked
        );
        assert_eq!(
            r.workstream("ui").unwrap().status,
            WorkstreamStatus::Blocked
        );
    }

    #[test]
    fn apply_change_adds_removes_and_guards() {
        let mk = || {
            ranch(vec![
                ws("schema", &[], WorkstreamStatus::Complete, Some("s1")),
                ws("api", &["schema"], WorkstreamStatus::Planned, None),
            ])
        };

        // Add a new workstream → present + readiness recomputed.
        let mut r = mk();
        let new_ws = Workstream {
            id: "cache".into(),
            title: "Cache".into(),
            goal: "add caching".into(),
            depends_on: vec!["api".into()],
            status: WorkstreamStatus::Planned,
            session_id: None,
            branch: None,
            worktree_path: None,
            expected_artifacts: vec![],
            acceptance: vec![],
        };
        let msg = apply_change(
            &mut r,
            &ScopeChange::AddWorkstream {
                workstream: new_ws.clone(),
            },
        )
        .unwrap();
        assert!(msg.contains("cache"));
        assert!(r.workstream("cache").is_some());
        // Duplicate add is rejected.
        assert!(apply_change(&mut r, &ScopeChange::AddWorkstream { workstream: new_ws }).is_err());

        // Remove a not-started workstream → gone. But api is depended on by cache,
        // so removing api is refused; remove cache first.
        let mut r = mk();
        apply_change(
            &mut r,
            &ScopeChange::AddWorkstream {
                workstream: Workstream {
                    id: "cache".into(),
                    title: "Cache".into(),
                    goal: String::new(),
                    depends_on: vec!["api".into()],
                    status: WorkstreamStatus::Planned,
                    session_id: None,
                    branch: None,
                    worktree_path: None,
                    expected_artifacts: vec![],
                    acceptance: vec![],
                },
            },
        )
        .unwrap();
        assert!(
            apply_change(&mut r, &ScopeChange::RemoveWorkstream { id: "api".into() }).is_err(),
            "can't remove a dependency of another workstream"
        );
        apply_change(
            &mut r,
            &ScopeChange::RemoveWorkstream { id: "cache".into() },
        )
        .unwrap();
        assert!(r.workstream("cache").is_none());

        // Can't remove a completed (done) workstream.
        let mut r = mk();
        assert!(
            apply_change(
                &mut r,
                &ScopeChange::RemoveWorkstream {
                    id: "schema".into()
                }
            )
            .is_err(),
            "can't remove completed work"
        );

        // Note is a no-op that succeeds.
        let mut r = mk();
        assert!(apply_change(&mut r, &ScopeChange::Note).is_ok());
        assert_eq!(r.workstreams.len(), 2);
    }

    #[test]
    fn finished_workstream_holds_for_signoff_regardless_of_criteria() {
        // Whether or not it declares acceptance criteria or expected artifacts, a
        // finished workstream parks for sign-off — sign-off is always explicit.
        for with_criteria in [false, true] {
            let mut schema = ws("schema", &[], WorkstreamStatus::Running, Some("s1"));
            if with_criteria {
                schema.acceptance = vec!["migrations apply cleanly".into()];
            }
            let mut r = ranch(vec![
                schema,
                ws("api", &["schema"], WorkstreamStatus::Planned, None),
            ]);
            let rec = reconcile_and_pick(&mut r, &|sid| {
                (sid == "s1").then_some(SessionStatus::Completed)
            });
            assert_eq!(rec.awaiting_acceptance, vec!["schema"]);
            assert_eq!(
                r.workstream("schema").unwrap().status,
                WorkstreamStatus::WaitingForUser
            );
            // api stays blocked (its dep isn't signed off) and nothing is ready.
            assert!(rec.ready.is_empty());
            assert_eq!(
                r.workstream("api").unwrap().status,
                WorkstreamStatus::Blocked
            );
        }
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
        let rec = reconcile_and_pick(&mut r, &|_| Some(SessionStatus::Running));
        assert!(rec.ready.is_empty());
        assert!(rec.awaiting_acceptance.is_empty());
        assert_eq!(
            r.workstream("schema").unwrap().status,
            WorkstreamStatus::Running
        );
    }

    #[test]
    fn complete_marks_done_promotes_and_unblocks_dependents() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path();
        // schema is Running with a worktree session that published an artifact.
        let wt = root.join("wt-schema");
        let sdir = wt.join(".cowboy/sessions/s1");
        std::fs::create_dir_all(&sdir).unwrap();
        cowboy_core::artifact::add_in(
            &sdir,
            "s1",
            cowboy_core::artifact::ArtifactKind::Contract,
            "Schema",
            "TABLE users",
            None,
            1,
        )
        .unwrap();
        let mut schema = ws("schema", &[], WorkstreamStatus::Running, Some("s1"));
        schema.worktree_path = Some(wt);
        let mut r = ranch(vec![
            schema,
            ws("api", &["schema"], WorkstreamStatus::Planned, None),
        ]);
        r.id = "billing".into();
        ranch::save(root, &r).unwrap();

        complete(root, "billing", "schema").unwrap();

        let r2 = ranch::load(root, "billing").unwrap();
        assert_eq!(
            r2.workstream("schema").unwrap().status,
            WorkstreamStatus::Complete
        );
        assert_eq!(
            r2.workstream("api").unwrap().status,
            WorkstreamStatus::Ready
        );
        assert!(
            cowboy_core::ranch::ranch_artifact_dir(root, "billing", "schema")
                .join("a0001-schema.md")
                .exists()
        );
    }

    #[test]
    fn promote_copies_session_artifacts_and_handoff_into_the_ranch_store() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path();
        // Simulate a completed workstream's worktree + session dir with outputs.
        let wt = root.join("wt");
        let session_dir = wt.join(".cowboy/sessions/sess1");
        std::fs::create_dir_all(&session_dir).unwrap();
        cowboy_core::artifact::add_in(
            &session_dir,
            "sess1",
            cowboy_core::artifact::ArtifactKind::Contract,
            "Schema",
            "TABLE users",
            None,
            1,
        )
        .unwrap();
        std::fs::write(session_dir.join("handoff.md"), "# Handoff\ndone").unwrap();

        let r = ranch(vec![]);
        let mut w = ws("schema", &[], WorkstreamStatus::Complete, Some("sess1"));
        w.worktree_path = Some(wt.clone());
        let n = promote_artifacts(root, &r, &w);
        assert_eq!(n, 1, "one artifact promoted");

        let dest = cowboy_core::ranch::ranch_artifact_dir(root, &r.id, "schema");
        assert!(dest.join("a0001-schema.md").exists(), "artifact copied");
        assert!(dest.join("handoff.md").exists(), "handoff copied");
    }

    #[test]
    fn compose_task_includes_goal_rules_and_dependency_artifacts() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let r = ranch(vec![]);
        // A dependency (schema) already promoted a contract into the ranch store.
        let dep_dir = cowboy_core::ranch::ranch_artifact_dir(tmp.path(), &r.id, "schema");
        std::fs::create_dir_all(&dep_dir).unwrap();
        std::fs::write(dep_dir.join("a0001-contract.md"), "# Schema\nTABLE users").unwrap();

        let mut w = ws("api", &["schema"], WorkstreamStatus::Ready, None);
        w.acceptance = vec!["tests pass".into()];
        let task = compose_task(tmp.path(), &r, &w);

        assert!(task.contains("Your workstream: API (api)"));
        assert!(task.contains("Depends on (complete): schema"));
        assert!(task.contains("tests pass"));
        assert!(task.contains("Coordination rules"));
        // The dependency's promoted artifact is injected for consumption.
        assert!(task.contains("Artifacts from your dependencies"));
        assert!(task.contains("schema/a0001-contract.md"));
        assert!(task.contains("TABLE users"));
    }
}
