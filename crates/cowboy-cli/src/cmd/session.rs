//! The interactive/one-shot session engine: wire the model client, agent
//! runtime, the network control pipeline, and a UI (ratatui TUI on a terminal,
//! console otherwise) into the agent loop.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};
use cowboy_core::daemonproto::{DaemonReq, DaemonResp, LeaseMode};
use cowboy_core::model::OpenAiClient;
use cowboy_core::netproto::{ApprovalScope, NetworkAttempt, Verdict};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::tui::SessionCtx;
use crate::agent::{AgentLoop, ConsoleUi};
use crate::cmd::daemon;
use crate::net::control;
use crate::net::docker::CliDocker;
use crate::net::runtime::AgentRuntime;

pub async fn run(task: Option<String>, _one_shot: bool) -> Result<()> {
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
        // thin client that starts (or, later, reuses) the session and attaches.
        let (model_names, current_model) = models_and_default(&user_models, &project_models);
        daemon::ensure_running().await?;
        let resp = daemon::request(DaemonReq::StartSession {
            root: root.clone(),
            task: task.clone(),
            mode: LeaseMode::Exclusive,
        })
        .await
        .context("starting session via cowboyd")?;
        let (id, sock) = match resp {
            DaemonResp::Started { id, worker_sock } => (id, worker_sock),
            DaemonResp::Err { message } => anyhow::bail!(message),
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        };
        let intro = welcome_lines(&root, &resolved, Some(&id));
        let title = context_title(&root);
        let ctx = SessionCtx {
            root,
            models: model_names,
            current_model,
        };
        crate::cmd::attach::attach_socket(&sock, &title, intro, ctx)
    } else {
        // Non-interactive: prompt on stdin if no task was given; run in-process
        // (the daemon still tracks a lease in a later milestone). Asks fail closed.
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
        if let Some(l) = &logger {
            eprintln!("session: {}", l.id());
        }
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
        agent.run(&task).await?;
        agent.shutdown().await; // stop managed processes
        Ok(())
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

fn verdict_str(v: Verdict) -> &'static str {
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

fn log_network(
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

fn log_approval(
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
