//! The interactive/one-shot session engine: wire the model client, agent
//! runtime, the network control pipeline, and a UI (ratatui TUI on a terminal,
//! console otherwise) into the agent loop.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::mpsc::Sender as StdSender;

use anyhow::{Context, Result};
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};
use cowboy_core::model::OpenAiClient;
use cowboy_core::netproto::{ApprovalScope, NetworkAttempt, Verdict};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::tui::{run_event_loop, AgentCmd, SessionCtx, TuiUi, TurnCancel, UiEvent};
use crate::agent::{AgentLoop, ConsoleUi};
use crate::net::docker::CliDocker;
use crate::net::runtime::AgentRuntime;
use crate::net::{approvals, control};

/// Builds a model client (and its context window) by profile name, for the
/// `/model` command. Captures the host-owned providers + model configs.
type ModelResolver =
    Box<dyn Fn(&str) -> Result<(Box<dyn cowboy_core::model::ModelClient>, usize)> + Send>;

pub async fn run(task: Option<String>, _one_shot: bool) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);

    let security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    let agent_cfg = AgentConfig::load(&paths.agent).unwrap_or_default();

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

    let context_window = resolved.context_window as usize;
    let model = OpenAiClient::from_resolved(&resolved).context("building model client")?;
    let interactive = std::io::stdout().is_terminal();
    let logger = crate::session::SessionLogger::create(&root).ok();
    if let Some(l) = &logger {
        if !interactive {
            eprintln!("session: {}", l.id());
        }
    }
    let intro = welcome_lines(&root, &resolved, logger.as_ref().map(|l| l.id()));
    let runtime = AgentRuntime::new(Box::new(CliDocker::new()), root, security);

    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });

    let behavior = agent_cfg.agent;
    if interactive {
        // Merged model names + the active default, for the `/model` command.
        let mut model_names: Vec<String> = Vec::new();
        if let Some(u) = &user_models {
            model_names.extend(u.models.keys().cloned());
        }
        if let Some(p) = &project_models {
            model_names.extend(p.models.keys().cloned());
        }
        model_names.sort();
        model_names.dedup();
        let current_model = project_models
            .as_ref()
            .and_then(|p| p.default.clone())
            .or_else(|| user_models.as_ref().and_then(|u| u.default.clone()))
            .unwrap_or_default();

        // Resolver the agent thread calls on `/model <name>` to rebuild the
        // client (provider creds stay host-owned).
        let resolve: ModelResolver = {
            let providers = providers.clone();
            let user = user_models.clone();
            let project = project_models.clone();
            Box::new(move |name: &str| {
                let r = resolve_model(&providers, user.as_ref(), project.as_ref(), Some(name))?;
                let cw = r.context_window as usize;
                let client: Box<dyn cowboy_core::model::ModelClient> =
                    Box::new(OpenAiClient::from_resolved(&r)?);
                Ok((client, cw))
            })
        };

        // Load straight into the full-screen UI; the first message is entered
        // there (no pre-prompt). An empty task means "start on the welcome
        // screen and wait for input".
        run_tui(
            task.unwrap_or_default(),
            intro,
            Box::new(model),
            runtime,
            behavior,
            context_window,
            logger,
            model_names,
            current_model,
            resolve,
        )
    } else {
        // Non-interactive: prompt on stdin if no task was given; asks fail closed.
        let Some(task) = resolve_task(task)? else {
            println!("nothing to do.");
            return Ok(());
        };
        let sock = runtime.control_sock();
        let session_dir = logger.as_ref().map(|l| l.dir().to_path_buf());
        if let Some(sock) = sock {
            tokio::spawn(run_control_autodeny(sock, session_dir));
        }
        let mut ui = ConsoleUi::new();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime,
            behavior,
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

/// Run the conversational ratatui front-end: the agent runs a turn loop on a
/// dedicated thread (its own runtime), the main thread owns the terminal. The
/// conversation (and container) persist across turns until the user ends it.
#[allow(clippy::too_many_arguments)]
fn run_tui(
    task: String,
    intro: Vec<String>,
    model: Box<dyn cowboy_core::model::ModelClient>,
    runtime: AgentRuntime,
    behavior: cowboy_core::config::AgentBehavior,
    context_window: usize,
    logger: Option<crate::session::SessionLogger>,
    model_names: Vec<String>,
    current_model: String,
    resolve: ModelResolver,
) -> Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
    let turn_cancel: TurnCancel = std::sync::Arc::new(std::sync::Mutex::new(None));
    let agent_turn_cancel = turn_cancel.clone();
    let seed = (!task.is_empty()).then_some(task);
    let root = runtime.root().to_path_buf();
    // Persistent title surfaces the working directory + git branch.
    let title = context_title(&root);

    // Control pipeline on its OWN always-on thread. This binds the host control
    // socket immediately — before the gateway is ever started — and keeps it
    // responsive between turns, so network `ask`s always reach the approval
    // modal (the agent thread's runtime only runs during a turn).
    let session_dir = logger.as_ref().map(|l| l.dir().to_path_buf());
    if let Some(sock) = runtime.control_sock() {
        let control_tx = ui_tx.clone();
        let root = runtime.root().to_path_buf();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(run_control_tui(sock, root, session_dir, control_tx));
        });
    }

    let handle = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ui_tx.send(UiEvent::Notice(format!("runtime error: {e}")));
                let _ = ui_tx.send(UiEvent::Done);
                return;
            }
        };

        // Separate sender for turn/session signals (the agent holds &mut ui).
        let loop_tx = ui_tx.clone();
        let mut ui = TuiUi { tx: ui_tx };
        {
            let mut agent = AgentLoop::new(
                model,
                runtime,
                behavior,
                context_window,
                CancellationToken::new(),
                &mut ui,
            )
            .with_logger(logger);

            // One command per loop; the conversation persists in `agent`.
            while let Ok(cmd) = task_rx.recv() {
                match cmd {
                    AgentCmd::SwitchModel(name) => match resolve(&name) {
                        Ok((client, cw)) => {
                            agent.set_model(client, cw);
                            let _ = loop_tx.send(UiEvent::Notice(format!("model is now {name}")));
                        }
                        Err(e) => {
                            let _ =
                                loop_tx.send(UiEvent::Notice(format!("model switch failed: {e}")));
                        }
                    },
                    AgentCmd::Message(msg) => {
                        let tc = CancellationToken::new();
                        *agent_turn_cancel.lock().unwrap() = Some(tc.clone());
                        let _ = rt.block_on(agent.run_turn(&msg, tc));
                        *agent_turn_cancel.lock().unwrap() = None;
                        // Post-turn indicators: working-tree diff, processes, and
                        // the title (the branch may change, e.g. a commit).
                        let (diff, procs) = post_turn_indicators(agent.root());
                        let _ = loop_tx.send(UiEvent::DiffStat(diff));
                        let _ = loop_tx.send(UiEvent::Processes(procs));
                        let _ = loop_tx.send(UiEvent::Title(context_title(agent.root())));
                        let _ = loop_tx.send(UiEvent::TurnDone);
                    }
                }
            }
            rt.block_on(agent.shutdown()); // stop managed processes
            agent.finalize_session();
        }
        let _ = loop_tx.send(UiEvent::Done);
    });

    let session_ctx = SessionCtx {
        root,
        models: model_names,
        current_model,
    };
    run_event_loop(
        &title,
        intro,
        seed,
        ui_rx,
        task_tx,
        turn_cancel,
        session_ctx,
    )?;
    let _ = handle.join();
    Ok(())
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

/// Serve the control socket and route asks to the TUI, logging decisions.
async fn run_control_tui(
    sock: PathBuf,
    root: PathBuf,
    session_dir: Option<PathBuf>,
    ui_tx: StdSender<UiEvent>,
) {
    let (approvals_tx, mut approvals_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = control::serve(sock, approvals_tx, events_tx).await;
    });

    // Event consumer: log to network.jsonl + show in the activity log.
    let ev_dir = session_dir.clone();
    let ev_ui = ui_tx.clone();
    tokio::spawn(async move {
        while let Some((attempt, verdict, reason)) = events_rx.recv().await {
            log_network(&ev_dir, &attempt, verdict, &reason);
            let _ = ev_ui.send(UiEvent::NetEvent(format!(
                "{} {} ({reason})",
                verdict_str(verdict),
                attempt.label()
            )));
        }
    });

    // Approval consumer: prompt the user, persist project/global, reply.
    while let Some(req) = approvals_rx.recv().await {
        let (utx, urx) = tokio::sync::oneshot::channel();
        if ui_tx
            .send(UiEvent::Approval(req.attempt.label(), utx))
            .is_err()
        {
            let _ = req.reply.send((Verdict::Deny, ApprovalScope::Once));
            continue;
        }
        let (verdict, scope) = urx.await.unwrap_or((Verdict::Deny, ApprovalScope::Once));
        if verdict == Verdict::Allow
            && matches!(scope, ApprovalScope::Project | ApprovalScope::Global)
        {
            let _ = approvals::append(&root, &req.attempt);
        }
        log_approval(&session_dir, &req.attempt, verdict, scope);
        log_network(&session_dir, &req.attempt, verdict, "user decision");
        // Surface the decided ask in the activity pane (the gateway emits no
        // event for `ask` verdicts — the host owns the outcome).
        let _ = ui_tx.send(UiEvent::NetEvent(format!(
            "{} {} (you decided)",
            verdict_str(verdict),
            req.attempt.label()
        )));
        let _ = req.reply.send((verdict, scope));
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
