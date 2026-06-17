//! `cowboy x-session-worker` — the headless agent process behind a session.
//!
//! Owns its docker container + gateway (keyed by the worktree path) and runs the
//! [`AgentLoop`] with a [`SocketUi`], serving a per-session socket that clients
//! attach to. Survives client detach. Spawned by the daemon (or run directly for
//! testing). Not for interactive use.

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::config::{
    resolve_model, AgentConfig, ConfigPaths, ModelsConfig, ProvidersConfig, SecurityConfig,
};
use cowboy_core::daemonproto::{
    ClientMsg, DaemonReq, InterruptKind, LeaseMode, SessionInfo, SessionStatus, UiEventMsg,
};
use cowboy_core::model::{ModelClient, OpenAiClient};
use tokio_util::sync::CancellationToken;

use crate::agent::socket_ui::SocketUi;
use crate::agent::AgentLoop;
use crate::cmd::daemon;
use crate::cmd::session::{
    context_title, git_branch, log_approval, log_network, post_turn_indicators, verdict_str,
};
use crate::net::docker::CliDocker;
use crate::net::runtime::{container_name_for, project_hash, AgentRuntime};
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
    /// Continue a prior session: load its transcript as the starting history.
    pub resume: Option<String>,
    /// Ranch/workstream this session belongs to (set by `cowboy ranch start`).
    pub ranch_id: Option<String>,
    pub workstream_id: Option<String>,
}

use cowboy_core::time::now_ms;

/// Consecutive 5s heartbeats the daemon may be unreachable before an unattended
/// worker considers itself orphaned and shuts down (~20s — tolerates a daemon
/// restart while reaping true zombies).
const ORPHAN_GRACE_BEATS: u32 = 4;

pub async fn run(args: WorkerArgs) -> Result<()> {
    let root = std::fs::canonicalize(&args.root).unwrap_or(args.root.clone());
    let paths = ConfigPaths::for_root(&root);

    let mut security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    // Merge the user's personal credential overlay (global + per-repo, shared by
    // all of a repo's worktrees) and re-validate the combined config.
    cowboy_core::usersecrets::merge_into(&mut security, &crate::net::runtime::repo_key(&root));
    security
        .validate()
        .context("validating merged credential grants")?;
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
    // The crew planner model (or a COWBOY_MODEL override) is the session default;
    // a stale/unknown override falls back to the models.yaml default.
    let model_override = crate::cmd::crew::session_model_override();
    let mut resolved = resolve_model(
        &providers,
        user_models.as_ref(),
        project_models.as_ref(),
        model_override.as_deref(),
    )
    .or_else(|_| {
        resolve_model(
            &providers,
            user_models.as_ref(),
            project_models.as_ref(),
            None,
        )
    })?;
    // A crew-routed subagent may override the temperature per task type.
    if let Some(t) = crate::cmd::crew::temperature_override() {
        resolved.temperature = t;
    }
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
        blocked_reason: None,
        ranch_id: args.ranch_id.clone(),
        workstream_id: args.workstream_id.clone(),
    };

    let reg_info = info.clone();
    let (mut ui, mut cmd_rx) = SocketUi::bind(&sock, &journal, info).await?;
    let emitter = ui.clone(); // post-turn events without borrowing `ui`
    println!("{}", sock.display()); // so the daemon/manual client can locate it

    // Cancelled by the heartbeat watchdog if the daemon vanishes for good and no
    // client is attached — so an orphaned worker shuts itself down (container +
    // finalize) instead of lingering forever.
    let orphan = CancellationToken::new();

    // Register with the daemon + heartbeat (daemon-managed sessions only).
    if args.register {
        let _ = daemon::request(DaemonReq::RegisterWorker {
            info: reg_info.clone(),
        })
        .await;
        let hb_id = id.clone();
        let hb_ui = emitter.clone();
        let hb_info = reg_info;
        let hb_orphan = orphan.clone();
        tokio::spawn(async move {
            // Consecutive heartbeats the daemon has been unreachable.
            let mut unreachable = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let st = hb_ui.stats();
                let status = if st.blocked_reason.is_some() {
                    SessionStatus::Blocked
                } else {
                    SessionStatus::Running
                };
                let attached = hb_ui.attached();
                let resp = daemon::request(DaemonReq::UpdateSession {
                    id: hb_id.clone(),
                    status,
                    turn: st.turn,
                    tokens: st.tokens,
                    diffstat: st.diffstat,
                    attached_clients: attached,
                    running_command: st.running_command,
                    branch: None,
                    blocked_reason: st.blocked_reason,
                })
                .await;
                match resp {
                    // Daemon is up but forgot us (it restarted / was cleaned) —
                    // re-register so a surviving worker is re-adopted. Reachable.
                    Ok(cowboy_core::daemonproto::DaemonResp::Err { .. }) => {
                        unreachable = 0;
                        let _ = daemon::request(DaemonReq::RegisterWorker {
                            info: hb_info.clone(),
                        })
                        .await;
                    }
                    Ok(_) => unreachable = 0,
                    // Connection error → the daemon is down. If it stays down past
                    // the grace window and no client is attached, this session is
                    // unreachable; shut down rather than orphan a zombie worker.
                    Err(_) => {
                        unreachable += 1;
                        if unreachable >= ORPHAN_GRACE_BEATS && attached == 0 {
                            tracing::warn!(
                                "daemon unreachable for {unreachable} beats and no client \
                                 attached; shutting down orphaned worker"
                            );
                            hb_orphan.cancel();
                            return;
                        }
                    }
                }
            }
        });
    }

    // Gate credential grants flagged `approval: required` behind a per-session
    // prompt to the attached client; a denied grant is dropped before the
    // container is ever built. With no client attached, this fails closed.
    gate_credential_grants(&mut security, &emitter).await;

    let runtime = AgentRuntime::new(Box::new(CliDocker::new()), root.clone(), security)?;
    // Captured before the runtime moves into the agent loop, so we can reap this
    // session's container + gateway on clean shutdown below.
    let container_name = runtime.container_name().to_string();

    // Network approvals + gateway events flow over the TCP control channel. Route
    // approvals to attached clients (fail closed with none); log + surface
    // decisions. Bound before the first turn so the gateway has a listener.
    if let Some((ctrl_addr, ctrl_token)) = runtime.control_endpoint() {
        tokio::spawn(run_control_pipeline(
            ctrl_addr,
            ctrl_token,
            emitter.clone(),
            Some(session_dir.clone()),
            root.clone(),
        ));
    }

    let memory_ctx = cowboy_core::memory::index(&format!("{:08x}", project_hash(&root)));
    // Continue a prior session if asked (load its transcript as history).
    let history = match &args.resume {
        Some(id) => match crate::session::load_history(&root, id) {
            Ok(h) => {
                ui.emit(UiEventMsg::Notice(format!(
                    "continuing session {id} ({} prior messages)",
                    h.len()
                )));
                h
            }
            Err(e) => {
                ui.emit(UiEventMsg::Notice(format!("could not resume {id}: {e}")));
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let mut agent = AgentLoop::new(
        Box::new(model),
        runtime,
        agent_cfg.agent,
        context_window,
        CancellationToken::new(),
        &mut ui,
    )
    .with_logger(logger)
    .with_memory_context(memory_ctx)
    .with_history(history)
    .with_pricing(resolved.input_cost_per_mtok, resolved.output_cost_per_mtok);

    // Connect this session to any user-configured MCP servers (host-side trusted
    // integrations). Adds the `mcp` tool + lists the servers in the system prompt;
    // connections are established lazily on first use, so this is cheap.
    let mut mcp_cfg = match cowboy_core::mcp::load_or_default() {
        Ok(cfg) => cfg,
        Err(e) => {
            emitter.emit(UiEventMsg::Notice(format!("MCP config error: {e}")));
            cowboy_core::mcp::McpConfig::default()
        }
    };
    // Fold in the repo's `.mcp.json` servers, but ONLY if the user has trusted them
    // (`cowboy mcp trust`). Host config wins on name collisions. An untrusted/stale
    // `.mcp.json` is skipped with a notice rather than silently running its commands.
    match crate::mcp::trust::project_trust(&root) {
        crate::mcp::trust::TrustState::Trusted => {
            use std::collections::btree_map::Entry;
            for (name, server) in crate::mcp::trust::trusted_servers(&root) {
                match mcp_cfg.servers.entry(name) {
                    Entry::Occupied(e) => emitter.emit(UiEventMsg::Notice(format!(
                        "MCP: repo server `{}` shadowed by your host config",
                        e.key()
                    ))),
                    Entry::Vacant(v) => {
                        v.insert(server);
                    }
                }
            }
        }
        crate::mcp::trust::TrustState::Untrusted | crate::mcp::trust::TrustState::Stale => {
            let n = cowboy_core::mcp::load_project_mcp(&root)
                .ok()
                .flatten()
                .map(|s| s.len())
                .unwrap_or(0);
            if n > 0 {
                emitter.emit(UiEventMsg::Notice(format!(
                    "this repo's .mcp.json defines {n} MCP server(s) — review and enable with \
                     `cowboy mcp trust`"
                )));
            }
        }
        crate::mcp::trust::TrustState::NoFile => {}
    }
    if mcp_cfg.any_enabled() {
        agent.enable_mcp(std::sync::Arc::new(crate::mcp::McpManager::new(mcp_cfg)));
    }

    // Rebuilds the model client for `/model <name>` (provider creds stay
    // host-owned; the agent only ever sees a built client).
    let resolve: Resolver = {
        let providers = providers.clone();
        let user = user_models.clone();
        let project = project_models.clone();
        Box::new(move |name: &str| {
            let r = resolve_model(&providers, user.as_ref(), project.as_ref(), Some(name))?;
            let cw = r.context_window as usize;
            let pricing = (r.input_cost_per_mtok, r.output_cost_per_mtok);
            let client: Box<dyn ModelClient> = Box::new(OpenAiClient::from_resolved(&r)?);
            Ok((client, cw, pricing))
        })
    };

    // Service client messages. A running turn is cancellable: `Interrupt`
    // cancels it (concurrently — control messages are read *while* the turn
    // runs), `End` stops the session, `SwitchModel` swaps the model, and extra
    // `Message`s queue behind the current turn.
    // A Ranch workstream worker is an ordinary interactive session: it runs its
    // seeded task to produce a first attempt, then idles for the user to attach,
    // refine, and sign off with `/accept` (`Accept` below) — which completes the
    // workstream and advances the plan. It does NOT auto-end on the first turn.
    // Expose the ranch context to the agent loop (the `propose_scope_change` tool
    // reads these to file proposals against the right ranch; absent outside a ranch).
    if let Some(rid) = &args.ranch_id {
        std::env::set_var("COWBOY_RANCH_ID", rid);
    }
    if let Some(wid) = &args.workstream_id {
        std::env::set_var("COWBOY_WORKSTREAM_ID", wid);
    }
    let mut queue: VecDeque<String> = VecDeque::new();
    if let Some(task) = args.task.clone() {
        queue.push_back(task);
    }
    'serve: loop {
        let next = match queue.pop_front() {
            Some(m) => m,
            None => {
                // Idle: wait for the next client message, or shut down if orphaned.
                // If we sit idle with no client attached past the configured
                // timeout, stop the container to free its RAM (the next command
                // restarts it); keep waiting so the session itself stays resumable.
                let idle_secs = agent.idle_container_timeout_seconds();
                let msg = loop {
                    let idle_tick = async {
                        if idle_secs == 0 {
                            std::future::pending::<()>().await
                        } else {
                            tokio::time::sleep(std::time::Duration::from_secs(idle_secs)).await
                        }
                    };
                    tokio::select! {
                        _ = orphan.cancelled() => break 'serve,
                        m = cmd_rx.recv() => break m,
                        _ = idle_tick => {
                            if emitter.attached() == 0 {
                                agent.stop_container().await;
                            }
                        }
                    }
                };
                match msg {
                    None => break,
                    Some(ClientMsg::Message(m)) => m,
                    Some(ClientMsg::End) => break,
                    Some(ClientMsg::SwitchModel(name)) => {
                        apply_switch(&mut agent, &resolve, &emitter, &name);
                        continue;
                    }
                    Some(ClientMsg::PlanMode(on)) => {
                        agent.set_planning(on);
                        emitter.emit(UiEventMsg::Notice(
                            if on {
                                "🧭 plan mode on — I'll research and propose a plan; \
                                 file edits are blocked until you approve with /go"
                            } else {
                                "▶ plan approved — executing"
                            }
                            .into(),
                        ));
                        continue;
                    }
                    // Sign off on this workstream: complete it + advance the plan,
                    // then end the session. A no-op (with a notice) outside a ranch.
                    Some(ClientMsg::Accept { note }) => {
                        sign_off(&id, note, &emitter).await;
                        break 'serve;
                    }
                    // No turn is running; interrupts and other control messages are
                    // no-ops.
                    _ => continue,
                }
            }
        };

        let mut end = false;
        let mut switch_to: Option<String> = None;
        {
            let tc = CancellationToken::new();
            let turn = run_turn(&mut agent, &emitter, &root, &next, tc.clone());
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    _ = &mut turn => break, // turn finished (emits TurnDone)
                    // Daemon gone + no client mid-turn → cancel and end the session.
                    _ = orphan.cancelled() => {
                        tc.cancel();
                        let _ = (&mut turn).await;
                        end = true;
                        break;
                    }
                    ctl = cmd_rx.recv() => match ctl {
                        None => {
                            tc.cancel();
                            let _ = (&mut turn).await;
                            end = true;
                            break;
                        }
                        Some(ClientMsg::Interrupt { kind }) => {
                            tc.cancel();
                            emitter.emit(UiEventMsg::Notice("interrupting current turn…".into()));
                            let _ = (&mut turn).await; // unwinds + emits TurnDone
                            match kind {
                                InterruptKind::End => end = true,
                                // Turn / Instruct: drop queued work, return to idle.
                                _ => queue.clear(),
                            }
                            break;
                        }
                        Some(ClientMsg::End) => {
                            tc.cancel();
                            let _ = (&mut turn).await;
                            end = true;
                            break;
                        }
                        // Queue further input to run after this turn.
                        Some(ClientMsg::Message(m)) => queue.push_back(m),
                        // Swapping the model needs &mut agent, so finish the
                        // current turn first, then apply below.
                        Some(ClientMsg::SwitchModel(n)) => {
                            tc.cancel();
                            let _ = (&mut turn).await;
                            switch_to = Some(n);
                            break;
                        }
                        // Sign off mid-turn: stop the turn, complete the workstream,
                        // and end the session.
                        Some(ClientMsg::Accept { note }) => {
                            tc.cancel();
                            let _ = (&mut turn).await;
                            sign_off(&id, note, &emitter).await;
                            end = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(n) = switch_to {
            apply_switch(&mut agent, &resolve, &emitter, &n);
        }
        if end {
            break 'serve;
        }
    }

    agent.shutdown().await;
    agent.finalize_session();
    emitter.end("session ended");
    if args.register {
        let _ = daemon::request(DaemonReq::CompleteSession { id: id.clone() }).await;
    }
    // Reap this session's container + gateway + networks now that it has ended.
    // We held the exclusive worktree lease, so we're the sole user; a *crashed*
    // worker's containers are instead reaped by the daemon's vacuum. (A detach
    // never reaches here — the worker keeps serving — so detached sessions live.)
    crate::cmd::down::teardown_project(&CliDocker::new(), &root, &container_name).await;
    Ok(())
}

/// Sign off on this session's ranch workstream: ask the daemon to complete the
/// workstream and advance the plan. The daemon resolves the ranch/workstream from
/// the session registry; outside a ranch (or if the workstream can't be accepted)
/// it returns an error we surface as a notice rather than failing the session.
async fn sign_off(id: &str, note: Option<String>, emitter: &SocketUi) {
    use cowboy_core::daemonproto::DaemonResp;
    let resp = daemon::request(DaemonReq::AcceptWorkstream {
        session: id.to_string(),
        note,
    })
    .await;
    let msg = match resp {
        Ok(DaemonResp::Accepted) => {
            "✓ signed off — workstream complete; advancing the plan".to_string()
        }
        Ok(DaemonResp::Err { message }) => format!("can't sign off: {message}"),
        Ok(other) => format!("unexpected daemon response: {other:?}"),
        Err(e) => format!("daemon not reachable: {e}"),
    };
    emitter.emit(UiEventMsg::Notice(msg));
}

/// Prompt the attached client to approve each credential grant marked
/// `approval: required` (whose host source is present) before it is exposed to
/// the container; drop denied grants from `security`. Fails closed: with no
/// client attached the prompt is denied, so the grant is not mounted/injected.
async fn gate_credential_grants(security: &mut cowboy_core::config::SecurityConfig, ui: &SocketUi) {
    use cowboy_core::config::expand_path;
    use cowboy_core::netproto::Verdict;

    // Only grants whose source is actually present can be exposed; prompting for
    // absent optional grants is pointless (they would be skipped anyway).
    let present_file = |f: &cowboy_core::config::SecretMount| {
        expand_path(&f.source).map(|p| p.exists()).unwrap_or(false)
    };
    let env_pending = security
        .secrets
        .env
        .iter()
        .any(|e| e.needs_approval() && std::env::var(&e.source_env).is_ok());
    let file_pending = security
        .secrets
        .files
        .iter()
        .any(|f| f.needs_approval() && present_file(f));
    if !env_pending && !file_pending {
        return;
    }

    // Give the interactive client a moment to attach so we don't auto-deny.
    ui.wait_for_client(std::time::Duration::from_secs(20)).await;

    let mut kept_env = Vec::with_capacity(security.secrets.env.len());
    for e in std::mem::take(&mut security.secrets.env) {
        if e.needs_approval() && std::env::var(&e.source_env).is_ok() {
            let prompt = format!(
                "credential: inject env {} (from ${}) into the container?",
                e.name, e.source_env
            );
            let (verdict, _scope) = ui.request_approval(prompt).await;
            if verdict == Verdict::Allow {
                kept_env.push(e);
            } else {
                ui.emit(UiEventMsg::Notice(format!(
                    "credential denied: env {} not injected",
                    e.name
                )));
            }
        } else {
            kept_env.push(e);
        }
    }
    security.secrets.env = kept_env;

    let mut kept_files = Vec::with_capacity(security.secrets.files.len());
    for f in std::mem::take(&mut security.secrets.files) {
        if f.needs_approval() && present_file(&f) {
            let mode = if f.read_only { "read-only" } else { "writable" };
            let prompt = format!(
                "credential: mount {} → {} ({mode}) into the container?",
                f.source, f.target
            );
            let (verdict, _scope) = ui.request_approval(prompt).await;
            if verdict == Verdict::Allow {
                kept_files.push(f);
            } else {
                ui.emit(UiEventMsg::Notice(format!(
                    "credential denied: {} not mounted",
                    f.source
                )));
            }
        } else {
            kept_files.push(f);
        }
    }
    security.secrets.files = kept_files;
}

/// Drive the host-side control socket for this session's gateway. Gateway
/// `event`s are logged + surfaced in the activity pane; `ask`s are routed to
/// attached clients via [`SocketUi::request_approval`] (fail closed with none),
/// approved project/global destinations are persisted, and the verdict is sent
/// back to the gateway. Approvals are handled serially to match the one-modal-
/// at-a-time TUI.
async fn run_control_pipeline(
    addr: String,
    token: String,
    ui: SocketUi,
    session_dir: Option<PathBuf>,
    root: PathBuf,
) {
    let (approvals_tx, mut approvals_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Bind the TCP control server on the gateway's bridge IP. That IP only
        // exists once the gateway network is created (lazily, on the first
        // command), so the address is initially unassignable — retry until it
        // appears rather than failing closed forever (which silently denies every
        // `ask`). The gateway itself retries connecting, so a brief gap is fine.
        let listener = loop {
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => break l, // serve_on logs "listening" below
                Err(e) => {
                    // EADDRNOTAVAIL until the bridge is up; log the first failure
                    // loudly, then quietly retry for the worker's lifetime.
                    tracing::debug!(%addr, error = %e, "control bind not ready; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        };
        let _ = control::serve_on(listener, token, approvals_tx, events_tx).await;
    });

    // Gateway-decided events: persist + show in the activity log.
    let ev_dir = session_dir.clone();
    let ev_ui = ui.clone();
    tokio::spawn(async move {
        // Destinations we've already explained, so a chatty blocked host doesn't
        // spam the transcript — the per-host count still climbs in the net pane.
        let mut explained: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some((attempt, verdict, reason)) = events_rx.recv().await {
            log_network(&ev_dir, &attempt, verdict, &reason);
            let dest = attempt.label();
            ev_ui.emit(UiEventMsg::NetEvent(format!(
                "{} {dest} ({reason})",
                verdict_str(verdict),
            )));
            // The first time a destination is blocked, surface a calm guardrail
            // notice in the transcript (not just the net pane) so a denied
            // request reads as "Cowboy protected you" with the fix — instead of
            // surfacing only as a raw connection error in the agent's output.
            if verdict == Verdict::Deny && explained.insert(dest.clone()) {
                let notice = if attempt.protocol == cowboy_core::netproto::Protocol::Dns {
                    let name = attempt.host.as_deref().unwrap_or(&dest);
                    format!(
                        "🛡 blocked DNS for {name} ({reason}). If that's expected, allow the \
                         domain in .cowboy/security.yaml (network_policy.allow.domains) — or, for \
                         a record type, network_policy.dns.allowed_qtypes."
                    )
                } else {
                    format!(
                        "🛡 blocked {dest} — not allowed by this project's network policy. \
                         If that's expected, allow it in .cowboy/security.yaml (network.allow), \
                         or re-run and approve when prompted."
                    )
                };
                ev_ui.emit(UiEventMsg::Notice(notice));
            }
        }
    });

    // Approvals: ask clients, persist project/global allows, reply to gateway.
    while let Some(req) = approvals_rx.recv().await {
        let dest = req.attempt.label();
        // Surface *why* we're asking (e.g. a suspected DNS tunnel) in the prompt.
        let prompt = match &req.reason {
            Some(r) => format!("{dest} — {r}"),
            None => dest.clone(),
        };
        let (verdict, scope) = ui.request_approval(prompt).await;
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

/// Rebuilds a model client by name (host-owned creds in, built client out).
/// Yields the client, its context window, and its (input, output) per-1M-token
/// USD pricing for the cost estimate.
type Resolver =
    Box<dyn Fn(&str) -> Result<(Box<dyn ModelClient>, usize, (Option<f64>, Option<f64>))>>;

/// Apply a `/model` switch: re-resolve and swap the client, or report why not.
fn apply_switch(agent: &mut AgentLoop<'_>, resolve: &Resolver, ui: &SocketUi, name: &str) {
    match resolve(name) {
        Ok((client, cw, (price_in, price_out))) => {
            agent.set_model(client, cw, price_in, price_out);
            ui.emit(UiEventMsg::Notice(format!("switched to model {name}")));
        }
        Err(e) => ui.emit(UiEventMsg::Notice(format!("model switch failed: {e}"))),
    }
}

/// Run one turn under `tc` and emit the post-turn indicators (diff, processes,
/// title). Returns when the turn completes or `tc` is cancelled.
async fn run_turn(
    agent: &mut AgentLoop<'_>,
    ui: &SocketUi,
    root: &std::path::Path,
    msg: &str,
    tc: CancellationToken,
) {
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
    use tokio::net::{TcpStream, UnixStream};

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
            blocked_reason: None,
            ranch_id: None,
            workstream_id: None,
        }
    }

    async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    }

    /// Reserve an ephemeral loopback addr for the TCP control server (bound by
    /// `run_control_pipeline`); the listener is dropped so the pipeline can bind it.
    fn free_control_addr() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("127.0.0.1:{}", l.local_addr().unwrap().port())
    }

    /// Connect a fake gateway to the control server and authenticate.
    async fn connect_gateway(
        addr: &str,
        token: &str,
    ) -> (
        BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = TcpStream::connect(addr).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (r, mut w) = stream.expect("connect control server").into_split();
        let hello = GatewayMessage::Hello {
            token: token.to_string(),
        };
        w.write_all(encode_line(&hello).as_bytes()).await.unwrap();
        w.flush().await.unwrap();
        (BufReader::new(r), w)
    }

    /// End-to-end through the worker glue, no Docker: a gateway `Ask` reaches an
    /// attached client over the worker socket, the client's `ApprovalReply`
    /// becomes the gateway `Decision`. Proves [`run_control_pipeline`] bridges
    /// the control socket and the per-session socket.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_flows_gateway_to_client_to_gateway() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s.sock");
        let control_addr = free_control_addr();
        let journal = tmp.path().join("events.jsonl");

        let (ui, _cmd_rx) = SocketUi::bind(&worker_sock, &journal, sample_info())
            .await
            .unwrap();
        tokio::spawn(run_control_pipeline(
            control_addr.clone(),
            "tok".into(),
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

        // Connect a fake (authenticated) gateway to the control server.
        let (mut greader, mut gwr) = connect_gateway(&control_addr, "tok").await;

        // Gateway asks about a destination.
        let ask = GatewayMessage::Ask {
            id: 99,
            reason: None,
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

    /// A credential grant flagged `approval: required` is prompted to the
    /// attached client; a Deny drops it, an Allow keeps it. Grants without the
    /// flag pass through untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_approval_gates_grants() {
        use cowboy_core::config::{SecretMount, SecurityConfig};

        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s.sock");
        let journal = tmp.path().join("events.jsonl");
        // A present source the grant can point at (the dir itself exists).
        let src = tmp.path().to_string_lossy().into_owned();

        let (ui, _cmd_rx) = SocketUi::bind(&worker_sock, &journal, sample_info())
            .await
            .unwrap();

        // Attach a client and handshake.
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

        let mut security = SecurityConfig::default();
        security.secrets.files = vec![
            SecretMount {
                source: src.clone(),
                target: "/tmp/.config/approved".into(),
                read_only: true,
                required: false,
                approval: Some("required".into()),
            },
            SecretMount {
                source: src.clone(),
                target: "/tmp/.config/denied".into(),
                read_only: true,
                required: false,
                approval: Some("required".into()),
            },
            SecretMount {
                source: src.clone(),
                target: "/tmp/.config/free".into(),
                read_only: true,
                required: false,
                approval: None, // no prompt
            },
        ];

        let gate_ui = ui.clone();
        let gate = tokio::spawn(async move {
            gate_credential_grants(&mut security, &gate_ui).await;
            security
        });

        // Answer two prompts: Allow the first, Deny the second.
        for _ in 0..2 {
            let (id, dest) = loop {
                let line = read_line(&mut creader).await;
                if let Ok(ServerMsg::Approval { id, dest }) = serde_json::from_str(line.trim()) {
                    break (id, dest);
                }
            };
            let verdict = if dest.contains("approved") {
                Verdict::Allow
            } else {
                Verdict::Deny
            };
            cw.write_all(
                encode_line(&ClientMsg::ApprovalReply {
                    id,
                    verdict,
                    scope: ApprovalScope::Session,
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            cw.flush().await.unwrap();
        }

        let security = gate.await.unwrap();
        let targets: Vec<_> = security
            .secrets
            .files
            .iter()
            .map(|f| f.target.as_str())
            .collect();
        assert!(targets.contains(&"/tmp/.config/approved"), "allowed kept");
        assert!(!targets.contains(&"/tmp/.config/denied"), "denied dropped");
        assert!(targets.contains(&"/tmp/.config/free"), "un-gated kept");
    }

    /// With no client attached, the gateway's ask is denied (fail closed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_denied_when_no_client_attached() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s.sock");
        let control_addr = free_control_addr();
        let journal = tmp.path().join("events.jsonl");

        let (ui, _cmd_rx) = SocketUi::bind(&worker_sock, &journal, sample_info())
            .await
            .unwrap();
        tokio::spawn(run_control_pipeline(
            control_addr.clone(),
            "tok".into(),
            ui.clone(),
            None,
            tmp.path().to_path_buf(),
        ));

        let (mut greader, mut gwr) = connect_gateway(&control_addr, "tok").await;

        let ask = GatewayMessage::Ask {
            id: 7,
            reason: None,
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

    /// A blocked destination produces one calm guardrail notice in the journal
    /// (deduped per host); an allowed one produces none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_destination_gets_one_guardrail_notice() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let worker_sock = tmp.path().join("s.sock");
        let control_addr = free_control_addr();
        let journal = tmp.path().join("events.jsonl");

        let (ui, _cmd_rx) = SocketUi::bind(&worker_sock, &journal, sample_info())
            .await
            .unwrap();
        tokio::spawn(run_control_pipeline(
            control_addr.clone(),
            "tok".into(),
            ui.clone(),
            Some(tmp.path().to_path_buf()),
            tmp.path().to_path_buf(),
        ));

        let (_greader, mut gwr) = connect_gateway(&control_addr, "tok").await;

        let attempt = |h: &str| NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some(h.into()),
            ip: None,
            port: 443,
        };
        // Two denies to the same host (should explain once) + one allow (silent).
        for msg in [
            GatewayMessage::Event {
                attempt: attempt("evil.test"),
                verdict: Verdict::Deny,
                reason: "not allowed".into(),
            },
            GatewayMessage::Event {
                attempt: attempt("evil.test"),
                verdict: Verdict::Deny,
                reason: "not allowed".into(),
            },
            GatewayMessage::Event {
                attempt: attempt("github.com"),
                verdict: Verdict::Allow,
                reason: "allow-list".into(),
            },
        ] {
            gwr.write_all(encode_line(&msg).as_bytes()).await.unwrap();
        }
        gwr.flush().await.unwrap();

        let mut body = String::new();
        for _ in 0..100 {
            body = std::fs::read_to_string(&journal).unwrap_or_default();
            if body.contains("🛡 blocked evil.test:443") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            body.matches("🛡 blocked evil.test:443").count(),
            1,
            "exactly one guardrail notice per blocked host"
        );
        assert!(
            !body.contains("🛡 blocked github.com"),
            "an allowed destination gets no guardrail notice"
        );
    }
}
