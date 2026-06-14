//! The interactive/one-shot session engine: wire the model client, agent
//! runtime, the network control pipeline, and a UI (ratatui TUI on a terminal,
//! console otherwise) into the agent loop.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};
use cowboy_core::daemonproto::{
    AttachTarget, DaemonReq, DaemonResp, LeaseMode, SessionInfo, SessionStatus,
};
use cowboy_core::model::OpenAiClient;
use cowboy_core::netproto::{ApprovalScope, NetworkAttempt, Verdict};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::tui::SessionCtx;
use crate::agent::{AgentLoop, ConsoleUi};
use crate::cli::StartFlags;
use crate::cmd::daemon;
use crate::net::control;
use crate::net::docker::CliDocker;
use crate::net::runtime::{container_name_for, AgentRuntime};

pub async fn run(task: Option<String>, flags: StartFlags) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);

    // Providers are host-owned (home dir); models may be user- or project-level.
    let providers = ProvidersConfig::load_global().context("loading providers.yaml")?;
    if providers.providers.is_empty() {
        anyhow::bail!("no model provider configured; run `cowboy models setup`");
    }
    let user_models = ModelsConfig::user_path()
        .map(|p| ModelsConfig::load_opt(&p))
        .transpose()?
        .flatten();
    let project_models = ModelsConfig::load_opt(&paths.models)?;
    let resolved = resolve_model(
        &providers,
        user_models.as_ref(),
        project_models.as_ref(),
        None,
    )?;

    let interactive = std::io::stdout().is_terminal();

    if interactive {
        // Interactive sessions run in a daemon-managed worker; this process is a
        // thin client that starts (or reuses) the session and attaches.
        let (model_names, current_model) = models_and_default(&user_models, &project_models);
        daemon::ensure_running().await?;
        let ctx_for = |root: PathBuf| SessionCtx {
            root,
            models: model_names.clone(),
            current_model: current_model.clone(),
        };
        let mut root = root;
        let mut force = flags.force;
        loop {
            let resp = daemon::request(DaemonReq::StartSession {
                root: root.clone(),
                task: task.clone(),
                mode: LeaseMode::Exclusive,
                force,
            })
            .await
            .context("starting session via cowboyd")?;
            match resp {
                DaemonResp::Started { id, worker_sock } => {
                    let intro = welcome_lines(&root, &resolved, Some(&id));
                    let title = context_title(&root);
                    return crate::cmd::attach::attach_socket(
                        &worker_sock,
                        &title,
                        intro,
                        ctx_for(root),
                    );
                }
                DaemonResp::LeaseDenied { held_by, .. } => {
                    match decide_collision(&held_by, &flags)? {
                        Collision::Attach => {
                            return attach_existing(&held_by.id, ctx_for(held_by.root), false)
                                .await;
                        }
                        Collision::ReadOnly => {
                            return attach_existing(&held_by.id, ctx_for(held_by.root), true).await;
                        }
                        Collision::NewWorktree => {
                            root = create_worktree_for(&root, task.as_deref()).await?;
                            force = false;
                            continue;
                        }
                        Collision::Force => {
                            force = true;
                            continue;
                        }
                        Collision::Quit => return Ok(()),
                    }
                }
                DaemonResp::Err { message } => anyhow::bail!(message),
                other => anyhow::bail!("unexpected daemon response: {other:?}"),
            }
        }
    } else {
        // Non-interactive (piped) one-shot: run in-process with a console UI, but
        // coordinate through the daemon so it can't collide with another session
        // in the same worktree. Asks fail closed.
        let Some(task) = resolve_task(task)? else {
            println!("nothing to do.");
            return Ok(());
        };
        let security = SecurityConfig::load(&paths.security)
            .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
        let agent_cfg = AgentConfig::load(&paths.agent).unwrap_or_default();
        let context_window = resolved.context_window as usize;
        let model = OpenAiClient::from_resolved(&resolved).context("building model client")?;
        let logger = crate::session::SessionLogger::create(&root).ok();
        let id = logger
            .as_ref()
            .map(|l| l.id().to_string())
            .unwrap_or_else(|| format!("{}-{}", now_ms(), std::process::id()));
        if let Some(l) = &logger {
            eprintln!("session: {}", l.id());
        }

        // Take the worktree lease (bails if another session holds it). Best-effort:
        // if the daemon is unreachable we run uncoordinated rather than block.
        let coordinated = coordinate_oneshot(&root, &id, &task).await?;

        let runtime = AgentRuntime::new(Box::new(CliDocker::new()), root, security);
        let cancel = CancellationToken::new();
        let signal_cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_cancel.cancel();
            }
        });
        let sock = runtime.control_sock();
        let session_dir = logger.as_ref().map(|l| l.dir().to_path_buf());
        if let Some(sock) = sock {
            tokio::spawn(run_control_autodeny(sock, session_dir));
        }
        let mut ui = ConsoleUi::new();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            agent_cfg.agent,
            context_window,
            cancel,
            &mut ui,
        )
        .with_logger(logger);
        let result = agent.run(&task).await;
        agent.shutdown().await; // stop managed processes

        // Release the lease and record the outcome.
        if coordinated {
            let req = match &result {
                Ok(_) => DaemonReq::CompleteSession { id },
                Err(e) => DaemonReq::FailSession {
                    id,
                    error: e.to_string(),
                },
            };
            let _ = daemon::request(req).await;
        }
        result.map(|_| ())
    }
}

/// Register a one-shot (non-interactive) session with the daemon and take its
/// worktree lease, so a piped run can't collide with another session in the same
/// worktree. Returns `Ok(true)` when coordinated, `Ok(false)` when the daemon is
/// unavailable (proceed uncoordinated rather than block scripts), or `Err` when
/// the worktree is already in use.
async fn coordinate_oneshot(root: &std::path::Path, id: &str, task: &str) -> Result<bool> {
    if daemon::ensure_running().await.is_err() {
        return Ok(false);
    }
    let now = now_ms() as u64;
    let info = SessionInfo {
        id: id.to_string(),
        root: root.to_path_buf(),
        task: Some(task.to_string()),
        status: SessionStatus::Running,
        pid: Some(std::process::id()),
        branch: git_branch(root),
        container_name: Some(container_name_for(root)),
        worker_sock: None, // in-process: no socket to attach to
        journal_path: None,
        lease_mode: Some(LeaseMode::Exclusive),
        started_ms: now,
        last_heartbeat_ms: now,
        turn: 0,
        tokens: (0, 0),
        attached_clients: 0,
        diffstat: String::new(),
        running_command: None,
    };
    let _ = daemon::request(DaemonReq::RegisterWorker { info }).await;
    match daemon::request(DaemonReq::AcquireLease {
        key: root.to_path_buf(),
        session: id.to_string(),
        mode: LeaseMode::Exclusive,
    })
    .await
    {
        Ok(DaemonResp::LeaseGranted { .. }) => Ok(true),
        Ok(DaemonResp::LeaseDenied { held_by, .. }) => {
            let _ = daemon::request(DaemonReq::FailSession {
                id: id.to_string(),
                error: "worktree busy".into(),
            })
            .await;
            anyhow::bail!(
                "worktree already has an active session ({}, {:?}); a non-interactive run \
                 can't share a worktree — run it from a separate git worktree \
                 (`cowboy worktree create`).",
                held_by.id,
                held_by.status
            );
        }
        // Daemon hiccup: don't block the run.
        _ => Ok(false),
    }
}

/// How a same-worktree collision was resolved.
enum Collision {
    /// Attach read-write to the active session.
    Attach,
    /// Attach read-only (watch without driving).
    ReadOnly,
    /// Create a fresh git worktree and run there.
    NewWorktree,
    /// Take over a stale lease (retry the start with `force`).
    Force,
    /// Do nothing.
    Quit,
}

/// Resolve a collision: honor an explicit flag, else prompt (interactive), else
/// fail safe with guidance. `--force` is rejected against a *live* holder.
fn decide_collision(held_by: &SessionInfo, flags: &StartFlags) -> Result<Collision> {
    // A terminal holder would have been reclaimed by the daemon; this is either
    // a live session or a stale one.
    let live = !matches!(
        held_by.status,
        cowboy_core::daemonproto::SessionStatus::Stale
    );

    if flags.attach_if_active {
        return Ok(Collision::Attach);
    }
    if flags.read_only {
        return Ok(Collision::ReadOnly);
    }
    if flags.new_worktree {
        return Ok(Collision::NewWorktree);
    }
    if flags.force {
        if live {
            anyhow::bail!(
                "session {} is live in this worktree; --force-same-worktree only takes over a \
                 *stale* lease. Use --new-worktree or --attach-if-active.",
                held_by.id
            );
        }
        return Ok(Collision::Force);
    }

    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "worktree already has an active session ({}, {:?}); rerun with --attach-if-active, \
             --read-only, --new-worktree{}",
            held_by.id,
            held_by.status,
            if live {
                ""
            } else {
                ", or --force-same-worktree"
            }
        );
    }
    prompt_collision(held_by, live)
}

/// Interactive collision menu (the session has not entered the TUI yet, so this
/// is plain stdin/stdout).
fn prompt_collision(held_by: &SessionInfo, live: bool) -> Result<Collision> {
    loop {
        println!("\nThis worktree already has an active session:");
        println!("  id      {}", held_by.id);
        println!("  status  {:?}", held_by.status);
        if let Some(b) = &held_by.branch {
            println!("  branch  {b}");
        }
        print!(
            "[a]ttach  [r]ead-only  [w] new worktree{}  [q]uit > ",
            if live { "" } else { "  [f]orce-takeover" }
        );
        io::stdout().flush().ok();

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(Collision::Quit);
        }
        match line.trim() {
            "a" | "attach" => return Ok(Collision::Attach),
            "r" | "ro" | "read-only" => return Ok(Collision::ReadOnly),
            "w" | "new" | "worktree" => return Ok(Collision::NewWorktree),
            "f" | "force" if !live => return Ok(Collision::Force),
            "q" | "quit" | "" => return Ok(Collision::Quit),
            other => println!("unrecognized choice: {other:?}"),
        }
    }
}

/// Attach to an existing session by id (live or, if it has since ended, replay).
async fn attach_existing(id: &str, ctx: SessionCtx, read_only: bool) -> Result<()> {
    let resp = daemon::request(DaemonReq::AttachSession { id: id.to_string() })
        .await
        .context("attaching via cowboyd")?;
    let title = context_title(&ctx.root);
    match resp {
        DaemonResp::Attach {
            target: AttachTarget::Live { worker_sock },
        } => {
            let intro = vec![format!(
                "attached to {id}{}",
                if read_only { " (read-only)" } else { "" }
            )];
            crate::cmd::attach::attach_socket_ro(&worker_sock, &title, intro, ctx, read_only)
        }
        DaemonResp::Attach {
            target:
                AttachTarget::Replay {
                    journal_path,
                    status,
                },
        } => crate::cmd::attach::replay_journal(&journal_path, &title, &format!("{status:?}"), ctx),
        DaemonResp::Err { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    }
}

/// Ask the daemon to create a new worktree off `root` for `task` and return its
/// path. Warns first if the base repo has uncommitted changes (they don't carry
/// into the new worktree, which checks out committed HEAD).
async fn create_worktree_for(root: &std::path::Path, task: Option<&str>) -> Result<PathBuf> {
    if crate::net::worktree::is_dirty(root) {
        println!(
            "warning: the current worktree has uncommitted changes; the new worktree \
             will start from the last commit (those changes won't carry over)."
        );
    }
    let branch = format!("cowboy/{}", crate::net::worktree::slugify(task));
    let resp = daemon::request(DaemonReq::CreateWorktree {
        repo: root.to_path_buf(),
        branch,
        path: None,
    })
    .await
    .context("creating worktree via cowboyd")?;
    match resp {
        DaemonResp::WorktreeCreated { path, branch } => {
            println!("created worktree {} on {branch}", path.display());
            Ok(path)
        }
        DaemonResp::Err { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    }
}

/// Merged model names (user + project) and the effective default, for `/model`.
fn models_and_default(
    user: &Option<ModelsConfig>,
    project: &Option<ModelsConfig>,
) -> (Vec<String>, String) {
    let mut names: Vec<String> = Vec::new();
    if let Some(u) = user {
        names.extend(u.models.keys().cloned());
    }
    if let Some(p) = project {
        names.extend(p.models.keys().cloned());
    }
    names.sort();
    names.dedup();
    let default = project
        .as_ref()
        .and_then(|p| p.default.clone())
        .or_else(|| user.as_ref().and_then(|u| u.default.clone()))
        .unwrap_or_default();
    (names, default)
}

/// Build the welcome-banner lines shown at the top of the TUI: project + model
/// context so a fresh session is oriented without a pre-prompt.
fn welcome_lines(
    root: &std::path::Path,
    model: &cowboy_core::config::ResolvedModel,
    session_id: Option<&str>,
) -> Vec<String> {
    let host = model
        .base_url
        .rsplit("://")
        .next()
        .unwrap_or(&model.base_url)
        .split('/')
        .next()
        .unwrap_or("");
    let mut lines = vec![
        "Welcome to cowboy — the agent runs sandboxed in Docker.".to_string(),
        format!("workspace  {}", root.display()),
        format!("model      {}  ({host})", model.model),
    ];
    if let Some(id) = session_id {
        lines.push(format!("session    {id}"));
    }
    let skills = cowboy_core::skills::discover(root).len();
    if skills > 0 {
        lines.push(format!(
            "skills     {skills} available (`cowboy skill list`)"
        ));
    }
    lines.push(String::new());
    lines.push(
        "Type a message to begin · Enter sends · PgUp/wheel scroll · Ctrl-C menu".to_string(),
    );
    lines
}

/// Persistent transcript title: the working directory (home-relative) plus the
/// git branch, e.g. `~/dev/cowboy  ⎇ main`.
pub(crate) fn context_title(root: &std::path::Path) -> String {
    let cwd = short_path(root);
    match git_branch(root) {
        Some(b) => format!("{cwd}  ⎇ {b}"),
        None => cwd,
    }
}

/// Display a path with the home directory collapsed to `~`.
fn short_path(p: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        if let Ok(rest) = p.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    p.display().to_string()
}

/// Current git branch of `root`, if it is a repository.
pub(crate) fn git_branch(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!b.is_empty()).then_some(b)
}

/// Resolve the task: use the provided one, or prompt for it (console mode only).
fn resolve_task(task: Option<String>) -> Result<Option<String>> {
    if let Some(t) = task {
        return Ok(Some(t));
    }
    use std::io::Write;
    print!("cowboy› what should I work on?\n> ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let t = line.trim().to_string();
    Ok((!t.is_empty()).then_some(t))
}

/// Compute the post-turn TUI indicators from the host side: a `git diff
/// --shortstat` summary (empty if not a repo / no changes) and the list of
/// managed processes (one `<name>.pid` file each under `.cowboy/proc/`).
pub(crate) fn post_turn_indicators(root: &std::path::Path) -> (String, Vec<(String, String)>) {
    let diff = std::process::Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["diff", "--shortstat"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            // "3 files changed, 30 insertions(+), 4 deletions(-)" -> "Δ 3f +30 -4"
            let num = |kw: &str| {
                s.split(',')
                    .find(|p| p.contains(kw))
                    .and_then(|p| p.split_whitespace().next())
                    .unwrap_or("0")
                    .to_string()
            };
            format!(
                "Δ {}f +{} -{}",
                num("file"),
                num("insertion"),
                num("deletion")
            )
        })
        .unwrap_or_default();

    let mut procs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join(".cowboy/proc")) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("pid") {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    procs.push((name.to_string(), "running".to_string()));
                }
            }
        }
        procs.sort();
    }
    (diff, procs)
}

// --- control pipeline ---

#[derive(Serialize)]
struct NetworkLogRecord<'a> {
    ts_ms: u128,
    dest: String,
    verdict: &'a str,
    reason: String,
}

#[derive(Serialize)]
struct ApprovalLogRecord {
    ts_ms: u128,
    dest: String,
    verdict: String,
    scope: String,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub(crate) fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::Ask => "ask",
    }
}

/// Non-interactive control pipeline: deny all asks (fail closed), log events.
async fn run_control_autodeny(sock: PathBuf, session_dir: Option<PathBuf>) {
    let (approvals_tx, mut approvals_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let serve_sock = sock.clone();
    tokio::spawn(async move {
        let _ = control::serve(serve_sock, approvals_tx, events_tx).await;
    });
    let ev_dir = session_dir.clone();
    tokio::spawn(async move {
        while let Some((attempt, verdict, reason)) = events_rx.recv().await {
            log_network(&ev_dir, &attempt, verdict, &reason);
        }
    });
    while let Some(req) = approvals_rx.recv().await {
        log_approval(
            &session_dir,
            &req.attempt,
            Verdict::Deny,
            ApprovalScope::Once,
        );
        log_network(
            &session_dir,
            &req.attempt,
            Verdict::Deny,
            "fail-closed (no approver)",
        );
        let _ = req.reply.send((Verdict::Deny, ApprovalScope::Once));
    }
}

pub(crate) fn log_network(
    session_dir: &Option<PathBuf>,
    attempt: &NetworkAttempt,
    verdict: Verdict,
    reason: &str,
) {
    if let Some(dir) = session_dir {
        crate::session::append_jsonl(
            &dir.join("network.jsonl"),
            &NetworkLogRecord {
                ts_ms: now_ms(),
                dest: attempt.label(),
                verdict: verdict_str(verdict),
                reason: reason.to_string(),
            },
        );
    }
}

pub(crate) fn log_approval(
    session_dir: &Option<PathBuf>,
    attempt: &NetworkAttempt,
    verdict: Verdict,
    scope: ApprovalScope,
) {
    if let Some(dir) = session_dir {
        crate::session::append_jsonl(
            &dir.join("approvals.jsonl"),
            &ApprovalLogRecord {
                ts_ms: now_ms(),
                dest: attempt.label(),
                verdict: verdict_str(verdict).to_string(),
                scope: format!("{scope:?}"),
            },
        );
    }
}
