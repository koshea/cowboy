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
use crate::cmd::session::{
    context_title, git_branch, log_approval, log_network, post_turn_indicators, verdict_str,
};
use crate::net::docker::CliDocker;
use crate::net::runtime::{container_name_for, AgentRuntime};
use crate::net::{approvals, control};
use cowboy_core::netproto::{ApprovalScope, Verdict};

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

    // Network approvals + gateway events flow over the control socket. Route
    // approvals to attached clients (fail closed with none); log + surface
    // decisions. Bound before the first turn so the gateway has a listener.
    if let Some(ctrl) = runtime.control_sock() {
        tokio::spawn(run_control_pipeline(
            ctrl,
            emitter.clone(),
            Some(session_dir.clone()),
            root.clone(),
        ));
    }

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
            // Ask/Approval replies are resolved inside SocketUi and never arrive
            // here. SwitchModel / Interrupt: later milestones.
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

/// Drive the host-side control socket for this session's gateway. Gateway
/// `event`s are logged + surfaced in the activity pane; `ask`s are routed to
/// attached clients via [`SocketUi::request_approval`] (fail closed with none),
/// approved project/global destinations are persisted, and the verdict is sent
/// back to the gateway. Approvals are handled serially to match the one-modal-
/// at-a-time TUI.
async fn run_control_pipeline(
    sock: PathBuf,
    ui: SocketUi,
    session_dir: Option<PathBuf>,
    root: PathBuf,
) {
    let (approvals_tx, mut approvals_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = control::serve(sock, approvals_tx, events_tx).await;
    });

    // Gateway-decided events: persist + show in the activity log.
    let ev_dir = session_dir.clone();
    let ev_ui = ui.clone();
    tokio::spawn(async move {
        while let Some((attempt, verdict, reason)) = events_rx.recv().await {
            log_network(&ev_dir, &attempt, verdict, &reason);
            ev_ui.emit(UiEventMsg::NetEvent(format!(
                "{} {} ({reason})",
                verdict_str(verdict),
                attempt.label()
            )));
        }
    });

    // Approvals: ask clients, persist project/global allows, reply to gateway.
    while let Some(req) = approvals_rx.recv().await {
        let dest = req.attempt.label();
        let (verdict, scope) = ui.request_approval(dest.clone()).await;
        if verdict == Verdict::Allow
            && matches!(scope, ApprovalScope::Project | ApprovalScope::Global)
        {
            let _ = approvals::append(&root, &req.attempt);
        }
        log_approval(&session_dir, &req.attempt, verdict, scope);
        log_network(&session_dir, &req.attempt, verdict, "user decision");
        ui.emit(UiEventMsg::NetEvent(format!(
            "{} {} (you decided)",
            verdict_str(verdict),
            dest
        )));
        let _ = req.reply.send((verdict, scope));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::daemonproto::{ClientMsg, ServerMsg, SessionInfo, SessionStatus};
    use cowboy_core::netproto::{
        encode_line, GatewayMessage, HostMessage, NetworkAttempt, Protocol,
    };
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    fn sample_info() -> SessionInfo {
        SessionInfo {
            id: "t".into(),
            root: "/tmp/x".into(),
            task: None,
            status: SessionStatus::Running,
            pid: None,
            branch: None,
            container_name: None,
            worker_sock: None,
            journal_path: None,
            lease_mode: None,
            started_ms: 0,
            last_heartbeat_ms: 0,
            turn: 0,
            tokens: (0, 0),
            attached_clients: 0,
            diffstat: String::new(),
            running_command: None,
        }
    }

    async fn read_line(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    }

    /// End-to-end through the worker glue, no Docker: a gateway `Ask` reaches an
    /// attached client over the worker socket, the client's `ApprovalReply`
    /// becomes the gateway `Decision`. Proves [`run_control_pipeline`] bridges
    /// the control socket and the per-session socket.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_flows_gateway_to_client_to_gateway() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s.sock");
        let control_sock = tmp.path().join("control.sock");
        let journal = tmp.path().join("events.jsonl");

        let (ui, _cmd_rx) = SocketUi::bind(&worker_sock, &journal, sample_info())
            .await
            .unwrap();
        tokio::spawn(run_control_pipeline(
            control_sock.clone(),
            ui.clone(),
            None,
            tmp.path().to_path_buf(),
        ));

        // Attach a client to the worker socket (handshake -> Snapshot).
        let client = UnixStream::connect(&worker_sock).await.unwrap();
        let (cr, mut cw) = client.into_split();
        let mut creader = BufReader::new(cr);
        cw.write_all(
            encode_line(&ClientMsg::Hello {
                since_seq: None,
                read_only: false,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        cw.flush().await.unwrap();
        assert!(read_line(&mut creader).await.contains("snapshot"));

        // Connect a fake gateway to the control socket (it appears slightly
        // after the pipeline spawns).
        let mut gw = None;
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(&control_sock).await {
                gw = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (gr, mut gwr) = gw.expect("connect control socket").into_split();
        let mut greader = BufReader::new(gr);

        // Gateway asks about a destination.
        let ask = GatewayMessage::Ask {
            id: 99,
            attempt: NetworkAttempt {
                protocol: Protocol::Tls,
                host: Some("example.com".into()),
                ip: None,
                port: 443,
            },
        };
        gwr.write_all(encode_line(&ask).as_bytes()).await.unwrap();
        gwr.flush().await.unwrap();

        // The client receives the approval prompt and allows it.
        let id = loop {
            let line = read_line(&mut creader).await;
            if let Ok(ServerMsg::Approval { id, dest }) = serde_json::from_str(line.trim()) {
                assert_eq!(dest, "example.com:443");
                break id;
            }
        };
        cw.write_all(
            encode_line(&ClientMsg::ApprovalReply {
                id,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            })
            .as_bytes(),
        )
        .await
        .unwrap();
        cw.flush().await.unwrap();

        // The gateway gets the matching Allow decision back.
        let decision: HostMessage =
            serde_json::from_str(read_line(&mut greader).await.trim()).unwrap();
        assert_eq!(
            decision,
            HostMessage::Decision {
                id: 99,
                verdict: Verdict::Allow,
                scope: ApprovalScope::Session,
            }
        );
    }

    /// With no client attached, the gateway's ask is denied (fail closed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_denied_when_no_client_attached() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s.sock");
        let control_sock = tmp.path().join("control.sock");
        let journal = tmp.path().join("events.jsonl");

        let (ui, _cmd_rx) = SocketUi::bind(&worker_sock, &journal, sample_info())
            .await
            .unwrap();
        tokio::spawn(run_control_pipeline(
            control_sock.clone(),
            ui.clone(),
            None,
            tmp.path().to_path_buf(),
        ));

        let mut gw = None;
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(&control_sock).await {
                gw = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (gr, mut gwr) = gw.expect("connect control socket").into_split();
        let mut greader = BufReader::new(gr);

        let ask = GatewayMessage::Ask {
            id: 7,
            attempt: NetworkAttempt {
                protocol: Protocol::Tls,
                host: Some("blocked.example".into()),
                ip: None,
                port: 443,
            },
        };
        gwr.write_all(encode_line(&ask).as_bytes()).await.unwrap();
        gwr.flush().await.unwrap();

        let decision: HostMessage =
            serde_json::from_str(read_line(&mut greader).await.trim()).unwrap();
        assert_eq!(
            decision,
            HostMessage::Decision {
                id: 7,
                verdict: Verdict::Deny,
                scope: ApprovalScope::Once,
            }
        );
    }
}
