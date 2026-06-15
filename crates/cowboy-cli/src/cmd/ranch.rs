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
use cowboy_core::ranch::{self, Ranch, RanchStatus, WorkstreamStatus};
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
        RanchCommand::Status { id } => status(&root, id),
        RanchCommand::Start { id } => start(&root, &id).await,
        RanchCommand::Attach { id, workstream } => attach(&root, &id, &workstream).await,
        RanchCommand::Complete { id, workstream } => complete(&root, &id, &workstream),
        RanchCommand::Watch { id } => watch(&root, &id).await,
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
    println!("✓ {workstream} marked complete — promoted {n} artifact(s)");
    if !newly.is_empty() {
        println!("newly ready: {}", newly.join(", "));
        println!("launch them with `cowboy ranch start {id}`.");
    }
    if ranch.status == RanchStatus::Complete {
        println!("ranch complete 🎉");
    }
    Ok(())
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

    // Promote the outputs of workstreams that just finished into the ranch store
    // (committed, shareable) so downstream workstreams + reviewers can use them.
    for ws_id in &reconciled.newly_complete {
        if let Some(ws) = ranch.workstream(ws_id).cloned() {
            let n = promote_artifacts(root, &ranch, &ws);
            log.push(format!("{ws_id} complete — promoted {n} artifact(s)"));
            if !ws.expected_artifacts.is_empty() && n == 0 {
                log.push(format!(
                    "  warning: {ws_id} declared expected artifacts ({}) but published none",
                    ws.expected_artifacts.join(", ")
                ));
            }
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
        log.push("nothing ready to start.".into());
    } else {
        log.push(format!("started {} workstream(s):", started.len()));
        for (wid, sid, branch) in &started {
            log.push(format!("  {wid}  → session {sid}  on {branch}"));
        }
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
    /// Ids that just transitioned to Complete (whose artifacts to promote).
    newly_complete: Vec<String>,
}

/// Reconcile already-started workstreams from their session status, recompute
/// readiness, and report what's ready + what just completed. Pure (status
/// lookup injected), so it's unit-testable without a daemon.
fn reconcile_and_pick(
    ranch: &mut Ranch,
    session_status: &dyn Fn(&str) -> Option<SessionStatus>,
) -> Reconciled {
    let mut newly_complete = Vec::new();
    for w in &mut ranch.workstreams {
        if matches!(
            w.status,
            WorkstreamStatus::Running | WorkstreamStatus::Starting
        ) {
            if let Some(sid) = &w.session_id {
                match session_status(sid) {
                    Some(SessionStatus::Completed) => {
                        w.status = WorkstreamStatus::Complete;
                        newly_complete.push(w.id.clone());
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
        newly_complete,
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
    let session_dir = wt.join(".cowboy").join("sessions").join(sid);
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
         - Do NOT edit the ranch plan; if it looks wrong, say so and stop rather than diverging.\n\
         - When done, publish the expected artifacts and a handoff, then finish.\n",
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
    fn reconcile_marks_finished_and_picks_newly_ready() {
        // schema is Running on session s1; api waits on schema; ui waits on api.
        let mut r = ranch(vec![
            ws("schema", &[], WorkstreamStatus::Running, Some("s1")),
            ws("api", &["schema"], WorkstreamStatus::Planned, None),
            ws("ui", &["api"], WorkstreamStatus::Planned, None),
        ]);
        // s1 has Completed → schema becomes Complete, api unblocks.
        let rec = reconcile_and_pick(&mut r, &|sid| {
            (sid == "s1").then_some(SessionStatus::Completed)
        });
        assert_eq!(rec.ready, vec!["api"]);
        assert_eq!(rec.newly_complete, vec!["schema"]);
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
        let rec = reconcile_and_pick(&mut r, &|_| Some(SessionStatus::Running));
        assert!(rec.ready.is_empty());
        assert!(rec.newly_complete.is_empty());
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
