//! `cowboy x-session-worker` — the headless agent process behind a session.
//!
//! Owns its docker container + gateway (keyed by the worktree path) and runs the
//! [`AgentLoop`] with a [`SocketUi`], serving a per-session socket that clients
//! attach to. Survives client detach. Spawned by the daemon (or run directly for
//! testing). Not for interactive use.

use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};
use cowboy_core::daemonproto::{
    ClientMsg, DaemonReq, LeaseMode, SessionInfo, SessionStatus, UiEventMsg,
};
use cowboy_core::model::OpenAiClient;
use tokio_util::sync::CancellationToken;

use crate::agent::socket_ui::SocketUi;
use crate::agent::AgentLoop;
use crate::cmd::daemon;
use crate::cmd::session::{context_title, git_branch, post_turn_indicators};
use crate::net::docker::CliDocker;
use crate::net::runtime::{container_name_for, AgentRuntime};

/// Args for the worker subcommand.
#[derive(Debug, Clone)]
pub struct WorkerArgs {
    pub root: PathBuf,
    pub task: Option<String>,
    /// Override the per-session socket path (defaults to runtime dir / s-<id>).
    pub sock: Option<PathBuf>,
    /// Daemon-assigned session id.
    pub id: Option<String>,
    /// Register with + heartbeat to the daemon.
    pub register: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn run(args: WorkerArgs) -> Result<()> {
    let root = std::fs::canonicalize(&args.root).unwrap_or(args.root.clone());
    let paths = ConfigPaths::for_root(&root);

    let security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    let agent_cfg = AgentConfig::load(&paths.agent).unwrap_or_default();

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

    let logger = match &args.id {
        Some(id) => crate::session::SessionLogger::create_with_id(&root, id).ok(),
        None => crate::session::SessionLogger::create(&root).ok(),
    };
    let id = logger
        .as_ref()
        .map(|l| l.id().to_string())
        .or_else(|| args.id.clone())
        .unwrap_or_else(|| format!("{}-{}", now_ms(), std::process::id()));
    let session_dir = logger
        .as_ref()
        .map(|l| l.dir().to_path_buf())
        .unwrap_or_else(|| root.join(".cowboy/sessions").join(&id));
    let journal = session_dir.join("events.jsonl");
    let sock = args
        .sock
        .clone()
        .unwrap_or_else(|| daemon::runtime_dir().join(format!("s-{id}.sock")));

    let info = SessionInfo {
        id: id.clone(),
        root: root.clone(),
        task: args.task.clone(),
        status: SessionStatus::Running,
        pid: Some(std::process::id()),
        branch: git_branch(&root),
        container_name: Some(container_name_for(&root)),
        worker_sock: Some(sock.clone()),
        journal_path: Some(journal.clone()),
        lease_mode: Some(LeaseMode::Exclusive),
        started_ms: now_ms(),
        last_heartbeat_ms: now_ms(),
        turn: 0,
        tokens: (0, 0),
        attached_clients: 0,
        diffstat: String::new(),
        running_command: None,
    };

    let reg_info = info.clone();
    let (mut ui, mut cmd_rx) = SocketUi::bind(&sock, &journal, info).await?;
    let emitter = ui.clone(); // post-turn events without borrowing `ui`
    println!("{}", sock.display()); // so the daemon/manual client can locate it

    // Register with the daemon + heartbeat (daemon-managed sessions only).
    if args.register {
        let _ = daemon::request(DaemonReq::RegisterWorker { info: reg_info }).await;
        let hb_id = id.clone();
        let hb_ui = emitter.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let _ = daemon::request(DaemonReq::UpdateSession {
                    id: hb_id.clone(),
                    status: SessionStatus::Running,
                    turn: 0,
                    tokens: (0, 0),
                    diffstat: String::new(),
                    attached_clients: hb_ui.attached(),
                    running_command: None,
                    branch: None,
                })
                .await;
            }
        });
    }

    let runtime = AgentRuntime::new(Box::new(CliDocker::new()), root.clone(), security);
    let mut agent = AgentLoop::new(
        Box::new(model),
        runtime,
        agent_cfg.agent,
        context_window,
        CancellationToken::new(),
        &mut ui,
    )
    .with_logger(logger);

    // Seed the initial task, then service client messages.
    if let Some(task) = args.task.clone() {
        run_turn(&mut agent, &emitter, &root, &task).await;
    }
    while let Some(msg) = cmd_rx.recv().await {
        match msg {
            ClientMsg::Message(m) => run_turn(&mut agent, &emitter, &root, &m).await,
            ClientMsg::End => break,
            // SwitchModel / Interrupt / Ask+Approval replies: later milestones.
            _ => {}
        }
    }

    agent.shutdown().await;
    agent.finalize_session();
    emitter.end("session ended");
    if args.register {
        let _ = daemon::request(DaemonReq::CompleteSession { id: id.clone() }).await;
    }
    Ok(())
}

/// Run one turn and emit the post-turn indicators (diff, processes, title).
async fn run_turn(agent: &mut AgentLoop<'_>, ui: &SocketUi, root: &std::path::Path, msg: &str) {
    let tc = CancellationToken::new();
    let _ = agent.run_turn(msg, tc).await;
    let (diff, procs) = post_turn_indicators(root);
    ui.emit(UiEventMsg::DiffStat(diff));
    ui.emit(UiEventMsg::Processes(procs));
    ui.emit(UiEventMsg::Title(context_title(root)));
    ui.emit(UiEventMsg::TurnDone);
}
