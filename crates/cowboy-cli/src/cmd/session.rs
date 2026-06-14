//! The interactive/one-shot session engine: wire the model client, agent
//! runtime, the network control pipeline, and a UI (ratatui TUI on a terminal,
//! console otherwise) into the agent loop.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::mpsc::Sender as StdSender;

use anyhow::{Context, Result};
use cowboy_core::config::{AgentConfig, ConfigPaths, ModelsConfig, SecurityConfig};
use cowboy_core::model::OpenAiClient;
use cowboy_core::netproto::{ApprovalScope, NetworkAttempt, Verdict};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::agent::tui::{run_event_loop, TuiUi, TurnCancel, UiEvent};
use crate::agent::{AgentLoop, ConsoleUi};
use crate::net::docker::CliDocker;
use crate::net::runtime::AgentRuntime;
use crate::net::{approvals, control};

pub async fn run(task: Option<String>, _one_shot: bool) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);

    let security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    let agent_cfg = AgentConfig::load(&paths.agent).unwrap_or_default();
    let models = ModelsConfig::load(&paths.models)
        .context("loading .cowboy/models.yaml (run `cowboy init` first)")?;
    let profile = models.resolve(None)?;

    let context_window = profile.context_window as usize;
    let model = OpenAiClient::from_profile(profile).context("building model client")?;
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

    let task = resolve_task(task)?;
    let Some(task) = task else {
        println!("nothing to do.");
        return Ok(());
    };

    let behavior = agent_cfg.agent;
    if std::io::stdout().is_terminal() {
        run_tui(
            task,
            Box::new(model),
            runtime,
            behavior,
            context_window,
            logger,
        )
    } else {
        // Non-interactive: asks fail closed; events are still logged.
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
        Ok(())
    }
}

/// Resolve the task: use the provided one, or prompt for it.
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
fn run_tui(
    task: String,
    model: Box<dyn cowboy_core::model::ModelClient>,
    runtime: AgentRuntime,
    behavior: cowboy_core::config::AgentBehavior,
    context_window: usize,
    logger: Option<crate::session::SessionLogger>,
) -> Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (task_tx, task_rx) = std::sync::mpsc::channel::<String>();
    let turn_cancel: TurnCancel = std::sync::Arc::new(std::sync::Mutex::new(None));
    let control_tx = ui_tx.clone();
    let agent_turn_cancel = turn_cancel.clone();
    let seed = (!task.is_empty()).then_some(task);

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

        // Start the control pipeline (asks -> TUI modal, events -> activity).
        let sock = runtime.control_sock();
        let root = runtime.root().to_path_buf();
        let session_dir = logger.as_ref().map(|l| l.dir().to_path_buf());
        if let Some(sock) = sock {
            rt.spawn(run_control_tui(sock, root, session_dir, control_tx));
        }

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

            // One turn per user message; the conversation persists in `agent`.
            while let Ok(msg) = task_rx.recv() {
                let tc = CancellationToken::new();
                *agent_turn_cancel.lock().unwrap() = Some(tc.clone());
                let _ = rt.block_on(agent.run_turn(&msg, tc));
                *agent_turn_cancel.lock().unwrap() = None;
                let _ = loop_tx.send(UiEvent::TurnDone);
            }
            agent.finalize_session();
        }
        let _ = loop_tx.send(UiEvent::Done);
    });

    run_event_loop("cowboy", seed, ui_rx, task_tx, turn_cancel)?;
    let _ = handle.join();
    Ok(())
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
