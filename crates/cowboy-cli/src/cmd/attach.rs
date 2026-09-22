//! `cowboy attach` — the thin client. Connects to a session worker's socket,
//! replays the journal, then streams live, reusing the existing ratatui
//! `run_event_loop` via a bridge that translates the wire protocol to/from the
//! in-process `UiEvent`/`AgentCmd` channels.
//!
//! The bridge is the heart of the client: a worker `ServerMsg` becomes a
//! `UiEvent` (with `Ask`/`Approval` reply channels synthesized locally and
//! their answers sent back as `ClientMsg`), and an `AgentCmd` from the UI
//! becomes a `ClientMsg` on the socket.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use cowboy_core::daemonproto::{
    AttachTarget, ClientMsg, DaemonReq, DaemonResp, InterruptKind, PendingPrompt, ServerMsg,
    SessionInfo, UiEventMsg,
};
use cowboy_core::netproto::encode_line;
use cowboy_tui::{Access, ConnectionState};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::agent::tui::{run_event_loop, AgentCmd, SessionCtx, TurnCancel, UiEvent, UiPrompt};

/// Attach to a session: by id (via the daemon) or, for testing, a worker socket
/// path directly.
pub async fn run(target: String) -> Result<()> {
    // Direct socket path (mainly for tests / debugging).
    let p = std::path::PathBuf::from(&target);
    if p.exists() && p.extension().is_some_and(|e| e == "sock") {
        let ctx = SessionCtx {
            root: std::env::current_dir().unwrap_or_default(),
            models: Vec::new(),
            current_model: String::new(),
            ranch_id: None,
            workstream_id: None,
            // Attaching to an existing session: the launchpad is for fresh starts.
            suggestions: Vec::new(),
        };
        return attach_socket(&p, "cowboy", Vec::new(), ctx);
    }

    // Otherwise resolve an exact or unambiguous session-id prefix against the
    // daemon's registry, then use the full id for both requests.
    let sessions = match crate::cmd::daemon::request(DaemonReq::ListSessions { root: None }).await {
        Ok(DaemonResp::Sessions { sessions }) => sessions,
        Ok(other) => anyhow::bail!("unexpected daemon response: {other:?}"),
        Err(e) => anyhow::bail!("cowboyd not reachable: {e}"),
    };
    let id = crate::session::replay::resolve_from(
        &target,
        sessions.iter().map(|session| session.id.as_str()),
    )?;
    let info = match crate::cmd::daemon::request(DaemonReq::GetSession { id: id.clone() }).await {
        Ok(DaemonResp::Session { info }) => info,
        Ok(DaemonResp::Err { message }) => anyhow::bail!(message),
        Ok(other) => anyhow::bail!("unexpected daemon response: {other:?}"),
        Err(e) => anyhow::bail!("cowboyd not reachable: {e}"),
    };
    let resp = crate::cmd::daemon::request(DaemonReq::AttachSession { id }).await?;
    let target = match resp {
        DaemonResp::Attach { target } => target,
        DaemonResp::Err { message } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon response: {other:?}"),
    };
    let ctx = SessionCtx {
        root: info.root.clone(),
        models: Vec::new(),
        current_model: String::new(),
        ranch_id: info.ranch_id.clone(),
        workstream_id: info.workstream_id.clone(),
        // Attaching to an existing session: the launchpad is for fresh starts.
        suggestions: Vec::new(),
    };
    let title = title_for(&info);
    match target {
        AttachTarget::Live { worker_sock } => {
            // A worker can exit between GetSession and our connect. Probe the
            // socket first; if it's dead, fall back to a read-only journal
            // replay rather than dropping the user into a broken live view.
            if crate::localsock::connect_blocking(&worker_sock).is_ok() {
                attach_socket(
                    &worker_sock,
                    &title,
                    vec![format!("attached to {}", info.id)],
                    ctx,
                )
            } else {
                match info.journal_path.clone() {
                    Some(j) => replay_journal(&j, &title, &format!("{:?}", info.status), ctx),
                    None => anyhow::bail!("session {} is gone and has no journal", info.id),
                }
            }
        }
        AttachTarget::Replay {
            journal_path,
            status,
        } => replay_journal(&journal_path, &title, &format!("{status:?}"), ctx),
    }
}

/// Render a terminal session read-only by replaying its `events.jsonl` from
/// disk. Loading the last event is not session completion: the viewer remains
/// navigable until the user exits with q/Esc.
pub fn replay_journal(
    journal_path: &std::path::Path,
    title: &str,
    status: &str,
    ctx: SessionCtx,
) -> Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    // The agent side is dead; nothing consumes commands, so the receiver is
    // dropped immediately and any input the user types is silently ignored.
    let (task_tx, _task_rx) = std::sync::mpsc::channel::<AgentCmd>();
    let turn_cancel: TurnCancel = std::sync::Arc::new(std::sync::Mutex::new(None));

    let events = crate::agent::socket_ui::read_journal(journal_path)?;
    let loop_tx = ui_tx.clone();
    let feeder = std::thread::spawn(move || {
        for event in events {
            if ui_tx.send(UiEvent::Wire(event)).is_err() {
                return;
            }
        }
    });

    let intro = vec![format!("replay of {status} session (read-only)")];
    run_event_loop(
        title,
        intro,
        None,
        ui_rx,
        loop_tx,
        task_tx,
        turn_cancel,
        Access::Replay,
        ctx,
    )?;
    let _ = feeder.join();
    Ok(())
}

/// Run the TUI attached to `sock`. The terminal event loop runs on this thread;
/// the bridge runs on its own thread with a tokio runtime.
pub fn attach_socket(
    sock: &std::path::Path,
    title: &str,
    intro: Vec<String>,
    ctx: SessionCtx,
) -> Result<()> {
    attach_socket_ro(sock, title, intro, ctx, false)
}

/// As [`attach_socket`], but `read_only` watches the session without driving it:
/// the client announces itself read-only and never forwards input.
pub fn attach_socket_ro(
    sock: &std::path::Path,
    title: &str,
    intro: Vec<String>,
    ctx: SessionCtx,
    read_only: bool,
) -> Result<()> {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
    let turn_cancel: TurnCancel =
        std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));

    // The event loop keeps a sender too (for client-side async results like the
    // fetched model list); the bridge thread takes its own clone.
    let loop_tx = ui_tx.clone();
    let sock = sock.to_path_buf();
    let bridge_cancel = turn_cancel.clone();
    let handle = std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            let _ = ui_tx.send(UiEvent::Connection(ConnectionState::Unavailable));
            return;
        };
        rt.block_on(async move {
            supervise_socket(&sock, ui_tx, task_rx, bridge_cancel, read_only).await;
        });
    });

    run_event_loop(
        title,
        intro,
        None,
        ui_rx,
        loop_tx,
        task_tx,
        turn_cancel,
        if read_only {
            Access::ReadOnlyLive
        } else {
            Access::Interactive
        },
        ctx,
    )?;
    let _ = handle.join();
    Ok(())
}

/// Keep one TUI/App attached across transport failures. Journal input resumes at
/// the first event the UI has actually accepted. Ordinary commands are attempted
/// once; ID-tagged prompt replies are retained only while snapshots say the prompt
/// is still pending.
async fn supervise_socket(
    sock: &std::path::Path,
    ui_tx: Sender<UiEvent>,
    task_rx: Receiver<AgentCmd>,
    turn_cancel: TurnCancel,
    read_only: bool,
) {
    supervise_socket_with_window(
        sock,
        ui_tx,
        task_rx,
        turn_cancel,
        read_only,
        Duration::from_secs(30),
    )
    .await;
}

async fn supervise_socket_with_window(
    sock: &std::path::Path,
    ui_tx: Sender<UiEvent>,
    mut task_rx: Receiver<AgentCmd>,
    turn_cancel: TurnCancel,
    read_only: bool,
    reconnect_window: Duration,
) {
    let (reply_tx, mut reply_rx) = unbounded_channel::<ClientMsg>();
    let mut next_seq: Option<u64> = None;
    let mut authoritative_prompts = HashSet::new();
    let mut pending_replies = HashMap::<u64, ClientMsg>::new();
    let mut reconnect_started = None;
    let mut attempt = 0u32;

    if ui_tx
        .send(UiEvent::Connection(ConnectionState::Connecting))
        .is_err()
    {
        return;
    }

    loop {
        let stream = match crate::localsock::connect(sock).await {
            Ok(stream) => stream,
            Err(_) => {
                if reconnect_or_stop(
                    &ui_tx,
                    &mut task_rx,
                    &mut reply_rx,
                    &authoritative_prompts,
                    &mut pending_replies,
                    &mut reconnect_started,
                    &mut attempt,
                    reconnect_window,
                )
                .await
                {
                    return;
                }
                continue;
            }
        };
        let (r, mut w) = stream.into_split();
        if !write_client(
            &mut w,
            &ClientMsg::Hello {
                since_seq: next_seq,
                read_only,
            },
        )
        .await
        {
            if reconnect_or_stop(
                &ui_tx,
                &mut task_rx,
                &mut reply_rx,
                &authoritative_prompts,
                &mut pending_replies,
                &mut reconnect_started,
                &mut attempt,
                reconnect_window,
            )
            .await
            {
                return;
            }
            continue;
        }

        let mut reader = BufReader::new(r);
        let mut line = String::new();
        let mut expected = next_seq.unwrap_or(0);
        let mut replay_boundary = None;
        let mut synced = false;
        let mut reconnect = false;

        while !reconnect {
            tokio::select! {
                read = reader.read_line(&mut line) => {
                    match read {
                        Ok(0) | Err(_) => {
                            reconnect = true;
                            continue;
                        }
                        Ok(_) => {}
                    }
                    let parsed = serde_json::from_str::<ServerMsg>(line.trim());
                    line.clear();
                    let Ok(msg) = parsed else {
                        reconnect = true;
                        continue;
                    };
                    if replay_boundary.is_none()
                        && !matches!(&msg, ServerMsg::Snapshot { .. } | ServerMsg::Ended { .. })
                    {
                        reconnect = true;
                        continue;
                    }
                    match msg {
                        ServerMsg::Snapshot {
                            info,
                            journal_len,
                            pending_prompts,
                        } => {
                            if replay_boundary.is_some() || journal_len < expected {
                                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(
                                    "invalid session replay boundary; reconnecting".into(),
                                )));
                                reconnect = true;
                                continue;
                            }
                            replay_boundary = Some(journal_len);
                            next_seq.get_or_insert(expected);
                            authoritative_prompts = pending_prompts
                                .iter()
                                .map(pending_prompt_id)
                                .collect();
                            pending_replies.retain(|id, _| authoritative_prompts.contains(id));
                            let replacement = if read_only {
                                Vec::new()
                            } else {
                                pending_prompts
                                    .into_iter()
                                    .map(|prompt| make_ui_prompt(prompt, &reply_tx))
                                    .collect()
                            };
                            if ui_tx
                                .send(UiEvent::Wire(UiEventMsg::Title(title_for(&info))))
                                .is_err()
                                || ui_tx.send(UiEvent::Lifecycle(info.status)).is_err()
                                || ui_tx.send(UiEvent::ReplacePrompts(replacement)).is_err()
                            {
                                return;
                            }
                            for reply in pending_replies.values() {
                                if !write_client(&mut w, reply).await {
                                    reconnect = true;
                                    break;
                                }
                            }
                            if !reconnect && expected == journal_len {
                                if ui_tx.send(UiEvent::Connection(ConnectionState::Live)).is_err() {
                                    return;
                                }
                                synced = true;
                                reconnect_started = None;
                                attempt = 0;
                            }
                        }
                        ServerMsg::Event { seq, event } => {
                            let Some(boundary) = replay_boundary else {
                                reconnect = true;
                                continue;
                            };
                            if seq != expected {
                                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(format!(
                                    "session event gap at {expected}; reconnecting"
                                ))));
                                reconnect = true;
                                continue;
                            }
                            // Commit the sequence only after the single long-lived UI
                            // accepted the event. A closed receiver means there is no
                            // consumer to reconnect on behalf of.
                            if ui_tx.send(UiEvent::Wire(event)).is_err() {
                                return;
                            }
                            expected += 1;
                            next_seq = Some(expected);
                            if !synced && expected == boundary {
                                if ui_tx.send(UiEvent::Connection(ConnectionState::Live)).is_err() {
                                    return;
                                }
                                synced = true;
                                reconnect_started = None;
                                attempt = 0;
                            }
                        }
                        ServerMsg::Ask { id, question, options } => {
                            if replay_boundary.is_none() {
                                reconnect = true;
                                continue;
                            }
                            authoritative_prompts.insert(id);
                            if !read_only {
                                let prompt = make_ui_prompt(
                                    PendingPrompt::Ask { id, question, options },
                                    &reply_tx,
                                );
                                if let UiPrompt::Ask { id, question, options, reply } = prompt {
                                    if ui_tx.send(UiEvent::Ask(id, question, options, reply)).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        ServerMsg::Approval { id, dest, detail } => {
                            if replay_boundary.is_none() {
                                reconnect = true;
                                continue;
                            }
                            authoritative_prompts.insert(id);
                            if !read_only {
                                let prompt = make_ui_prompt(
                                    PendingPrompt::Approval { id, dest, detail },
                                    &reply_tx,
                                );
                                if let UiPrompt::Approval { id, dest, detail, reply } = prompt {
                                    if ui_tx.send(UiEvent::Approval(id, dest, detail, reply)).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        ServerMsg::AskResolved { id } => {
                            authoritative_prompts.remove(&id);
                            pending_replies.remove(&id);
                            if ui_tx.send(UiEvent::AskResolved(id)).is_err() {
                                return;
                            }
                        }
                        ServerMsg::ApprovalResolved { id } => {
                            authoritative_prompts.remove(&id);
                            pending_replies.remove(&id);
                            if ui_tx.send(UiEvent::ApprovalResolved(id)).is_err() {
                                return;
                            }
                        }
                        ServerMsg::Status(status) => {
                            if ui_tx.send(UiEvent::Lifecycle(status)).is_err() {
                                return;
                            }
                        }
                        ServerMsg::Ended { reason } => {
                            let _ = ui_tx.send(UiEvent::Connection(ConnectionState::Ended {
                                reason: reason.clone(),
                            }));
                            if !reason.is_empty() {
                                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(reason)));
                            }
                            let _ = ui_tx.send(UiEvent::Done);
                            return;
                        }
                    }
                }
                reply = reply_rx.recv() => {
                    let Some(reply) = reply else { return };
                    let Some(id) = prompt_reply_id(&reply) else { continue };
                    if !authoritative_prompts.contains(&id) {
                        continue;
                    }
                    pending_replies.insert(id, reply.clone());
                    if !synced || !write_client(&mut w, &reply).await {
                        reconnect = true;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(25)) => {
                    match task_rx.try_recv() {
                        Ok(cmd) => {
                            let (msg, leave) = agent_command(cmd);
                            if read_only {
                                if leave {
                                    let _ = write_client(&mut w, &ClientMsg::Detach).await;
                                    return;
                                }
                                continue;
                            }
                            let wrote = write_client(&mut w, &msg).await;
                            if !wrote && !leave {
                                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(
                                    "connection dropped while sending a command; it was not replayed"
                                        .into(),
                                )));
                            }
                            if leave {
                                return;
                            }
                            if !wrote {
                                reconnect = true;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            let msg = if read_only { ClientMsg::Detach } else { ClientMsg::End };
                            let _ = write_client(&mut w, &msg).await;
                            return;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                    if !read_only {
                        let token = turn_cancel.lock().unwrap().clone();
                        if token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                            let msg = ClientMsg::Interrupt { kind: InterruptKind::Turn };
                            let wrote = write_client(&mut w, &msg).await;
                            *turn_cancel.lock().unwrap() = Some(CancellationToken::new());
                            if !wrote {
                                reconnect = true;
                            }
                        }
                    }
                }
            }
        }

        if reconnect_or_stop(
            &ui_tx,
            &mut task_rx,
            &mut reply_rx,
            &authoritative_prompts,
            &mut pending_replies,
            &mut reconnect_started,
            &mut attempt,
            reconnect_window,
        )
        .await
        {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconnect_or_stop(
    ui_tx: &Sender<UiEvent>,
    task_rx: &mut Receiver<AgentCmd>,
    reply_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientMsg>,
    authoritative_prompts: &HashSet<u64>,
    pending_replies: &mut HashMap<u64, ClientMsg>,
    reconnect_started: &mut Option<tokio::time::Instant>,
    attempt: &mut u32,
    reconnect_window: Duration,
) -> bool {
    let started = reconnect_started.get_or_insert_with(tokio::time::Instant::now);
    if started.elapsed() >= reconnect_window {
        let _ = ui_tx.send(UiEvent::Connection(ConnectionState::Unavailable));
        return wait_unavailable(task_rx, reply_rx, authoritative_prompts, pending_replies).await;
    }
    *attempt = attempt.saturating_add(1);
    if ui_tx
        .send(UiEvent::Connection(ConnectionState::Reconnecting {
            attempt: *attempt,
        }))
        .is_err()
    {
        return true;
    }
    let delay = reconnect_backoff(*attempt);
    let until = tokio::time::Instant::now() + delay;
    loop {
        if tokio::time::Instant::now() >= until {
            return false;
        }
        tokio::select! {
            reply = reply_rx.recv() => {
                let Some(reply) = reply else { return true };
                if let Some(id) = prompt_reply_id(&reply) {
                    if authoritative_prompts.contains(&id) {
                        pending_replies.insert(id, reply);
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                match task_rx.try_recv() {
                    Ok(AgentCmd::Detach | AgentCmd::End) => return true,
                    Ok(_) => {
                        let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(
                            "command was not sent while the session was reconnecting".into(),
                        )));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
        }
    }
}

async fn wait_unavailable(
    task_rx: &mut Receiver<AgentCmd>,
    reply_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientMsg>,
    authoritative_prompts: &HashSet<u64>,
    pending_replies: &mut HashMap<u64, ClientMsg>,
) -> bool {
    loop {
        tokio::select! {
            reply = reply_rx.recv() => {
                let Some(reply) = reply else { return true };
                if let Some(id) = prompt_reply_id(&reply) {
                    if authoritative_prompts.contains(&id) {
                        pending_replies.insert(id, reply);
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                match task_rx.try_recv() {
                    Ok(AgentCmd::Detach | AgentCmd::End) => return true,
                    Ok(_) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
        }
    }
}

async fn write_client(writer: &mut tokio::net::unix::OwnedWriteHalf, msg: &ClientMsg) -> bool {
    writer.write_all(encode_line(msg).as_bytes()).await.is_ok() && writer.flush().await.is_ok()
}

fn pending_prompt_id(prompt: &PendingPrompt) -> u64 {
    match prompt {
        PendingPrompt::Ask { id, .. } | PendingPrompt::Approval { id, .. } => *id,
    }
}

fn prompt_reply_id(msg: &ClientMsg) -> Option<u64> {
    match msg {
        ClientMsg::AskReply { id, .. } | ClientMsg::ApprovalReply { id, .. } => Some(*id),
        _ => None,
    }
}

fn make_ui_prompt(prompt: PendingPrompt, out_tx: &UnboundedSender<ClientMsg>) -> UiPrompt {
    match prompt {
        PendingPrompt::Ask {
            id,
            question,
            options,
        } => {
            let (reply, answers) = std::sync::mpsc::channel();
            let out = out_tx.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(answer) = answers.recv() {
                    let _ = out.send(ClientMsg::AskReply { id, answer });
                }
            });
            UiPrompt::Ask {
                id,
                question,
                options,
                reply,
            }
        }
        PendingPrompt::Approval { id, dest, detail } => {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let out = out_tx.clone();
            tokio::spawn(async move {
                if let Ok((verdict, scope)) = answer.await {
                    let _ = out.send(ClientMsg::ApprovalReply { id, verdict, scope });
                }
            });
            UiPrompt::Approval {
                id,
                dest,
                detail,
                reply,
            }
        }
    }
}

fn reconnect_backoff(attempt: u32) -> Duration {
    Duration::from_millis((250u64 << attempt.saturating_sub(1).min(3)).min(2_000))
}

fn agent_command(cmd: AgentCmd) -> (ClientMsg, bool) {
    match cmd {
        AgentCmd::Message(m) => (ClientMsg::Message(m), false),
        AgentCmd::Enqueue(m) => (ClientMsg::Enqueue(m), false),
        AgentCmd::QueueClear => (ClientMsg::QueueClear, false),
        AgentCmd::SwitchModel(n) => (ClientMsg::SwitchModel(n), false),
        AgentCmd::PlanMode(b) => (ClientMsg::PlanMode(b), false),
        AgentCmd::Accept { note } => (ClientMsg::Accept { note }, false),
        AgentCmd::StopSubagents => (ClientMsg::StopSubagents, false),
        AgentCmd::Detach => (ClientMsg::Detach, true),
        AgentCmd::End => (ClientMsg::End, true),
    }
}

/// Bridge a connected worker `stream` to the UI channels. Returns when the
/// worker ends or the UI hangs up. A `read_only` client never forwards input
/// (no `Message`/`SwitchModel`), so it can watch without driving the session.
pub async fn bridge(
    stream: UnixStream,
    ui_tx: Sender<UiEvent>,
    task_rx: Receiver<AgentCmd>,
    turn_cancel: TurnCancel,
    read_only: bool,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();

    // Subscribe from the start.
    out_tx
        .send(ClientMsg::Hello {
            since_seq: None,
            read_only,
        })
        .ok();

    // Single writer: drain ClientMsgs to the socket.
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if w.write_all(encode_line(&msg).as_bytes()).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
    });

    // A local detach (the user chose "detach" in the pause menu) leaves the
    // session running, so no `Ended` is coming. The reader would then block
    // forever waiting for the worker to close the socket, hanging the client's
    // `handle.join()`. cmd_pump signals this channel so the bridge tears itself
    // down immediately instead.
    let (detach_tx, mut detach_rx) = tokio::sync::oneshot::channel::<()>();

    // UI commands (blocking std recv) -> ClientMsg. On hangup, end the session.
    // A read-only client drops input but still drains the channel and ends on
    // hangup; it must never send `End` (that would stop a session it's only
    // watching).
    let cmd_out = out_tx.clone();
    let cmd_pump = tokio::task::spawn_blocking(move || {
        while let Ok(cmd) = task_rx.recv() {
            // An explicit detach leaves the session running; tell the worker, then
            // signal the bridge to exit without waiting for an `Ended`.
            if let AgentCmd::Detach = cmd {
                let _ = cmd_out.send(ClientMsg::Detach);
                let _ = detach_tx.send(());
                return;
            }
            if read_only {
                continue;
            }
            // An explicit end: forward it and stop pumping. Returning here rather
            // than falling through to the hangup path means exactly one `End` is
            // sent, whether the client asked for it or simply hung up.
            if let AgentCmd::End = cmd {
                let _ = cmd_out.send(ClientMsg::End);
                return;
            }
            let msg = match cmd {
                AgentCmd::Message(m) => ClientMsg::Message(m),
                AgentCmd::Enqueue(m) => ClientMsg::Enqueue(m),
                AgentCmd::QueueClear => ClientMsg::QueueClear,
                AgentCmd::SwitchModel(n) => ClientMsg::SwitchModel(n),
                AgentCmd::PlanMode(b) => ClientMsg::PlanMode(b),
                AgentCmd::Accept { note } => ClientMsg::Accept { note },
                AgentCmd::StopSubagents => ClientMsg::StopSubagents,
                AgentCmd::Detach | AgentCmd::End => unreachable!("handled above"),
            };
            if cmd_out.send(msg).is_err() {
                return;
            }
        }
        let _ = cmd_out.send(if read_only {
            ClientMsg::Detach
        } else {
            ClientMsg::End
        });
    });

    // Interrupt watcher: when the UI fires the turn-cancel token, send an
    // Interrupt and re-arm a fresh token for the next turn. Read-only clients
    // don't interrupt the session they're watching.
    let int_out = out_tx.clone();
    let int_cancel = turn_cancel.clone();
    let interrupts = tokio::spawn(async move {
        loop {
            let token = int_cancel.lock().unwrap().clone();
            let Some(token) = token else { break };
            token.cancelled().await;
            if !read_only
                && int_out
                    .send(ClientMsg::Interrupt {
                        kind: InterruptKind::Turn,
                    })
                    .is_err()
            {
                break;
            }
            *int_cancel.lock().unwrap() = Some(CancellationToken::new());
        }
    });

    // Reader: worker ServerMsg -> UiEvent (+ reply synthesis). Runs as a task so a
    // local detach can tear the bridge down without waiting on it — otherwise it
    // would block on `read_line` until the worker closes the socket, which never
    // happens for a detach (the session stays up).
    let read_out = out_tx.clone();
    let mut reader_task = tokio::spawn(async move {
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(msg) = serde_json::from_str::<ServerMsg>(line.trim()) else {
                continue;
            };
            if !handle_server_msg(msg, &ui_tx, &read_out) {
                let _ = ui_tx.send(UiEvent::Done);
                break; // Ended
            }
        }
    });

    // Exit when the worker ends/closes the socket, or when the user detaches.
    //
    // `Ok(())` matters: `detach_tx` lives in `cmd_pump`, so it is *dropped* whenever
    // that task returns — including right after it queues an `End`. A bare `_ =`
    // arm therefore fired on the drop, and the teardown below aborted the writer
    // before the `End` had been written to the socket. The user saw "session ended"
    // and a worker that kept running, because the request never left the client.
    // Only an explicit send is a detach; a drop means nothing.
    tokio::select! {
        _ = &mut reader_task => {}
        Ok(()) = &mut detach_rx => {}
    }

    // Stop the producers, then let the writer drain what they queued. Aborting it
    // outright was the other half of the bug above: the last message a client sends
    // is precisely the one that says why it is leaving, so dropping it strands the
    // session. Bounded, because a wedged socket must not hold the client open.
    reader_task.abort();
    interrupts.abort();
    cmd_pump.abort();
    drop(out_tx);
    if tokio::time::timeout(Duration::from_secs(2), &mut writer)
        .await
        .is_err()
    {
        tracing::debug!("outbound queue did not drain before teardown");
        writer.abort();
    }
    Ok(())
}

/// Translate one `ServerMsg` into UI events. Returns false when the session has
/// ended (the caller should stop reading).
fn handle_server_msg(
    msg: ServerMsg,
    ui_tx: &Sender<UiEvent>,
    out_tx: &UnboundedSender<ClientMsg>,
) -> bool {
    match msg {
        ServerMsg::Snapshot {
            info,
            pending_prompts,
            ..
        } => {
            let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Title(title_for(&info))));
            let _ = ui_tx.send(UiEvent::Lifecycle(info.status));
            let prompts = pending_prompts
                .into_iter()
                .map(|prompt| make_ui_prompt(prompt, out_tx))
                .collect();
            let _ = ui_tx.send(UiEvent::ReplacePrompts(prompts));
        }
        ServerMsg::Event { event, .. } => {
            let _ = ui_tx.send(UiEvent::Wire(event));
        }
        ServerMsg::Ask {
            id,
            question,
            options,
        } => {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel::<String>();
            let _ = ui_tx.send(UiEvent::Ask(id, question, options, reply_tx));
            let out = out_tx.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(answer) = reply_rx.recv() {
                    let _ = out.send(ClientMsg::AskReply { id, answer });
                }
            });
        }
        ServerMsg::Approval { id, dest, detail } => {
            let (vtx, vrx) = tokio::sync::oneshot::channel();
            let _ = ui_tx.send(UiEvent::Approval(id, dest, detail, vtx));
            let out = out_tx.clone();
            tokio::spawn(async move {
                if let Ok((verdict, scope)) = vrx.await {
                    let _ = out.send(ClientMsg::ApprovalReply { id, verdict, scope });
                }
            });
        }
        ServerMsg::AskResolved { id } => {
            let _ = ui_tx.send(UiEvent::AskResolved(id));
        }
        ServerMsg::ApprovalResolved { id } => {
            let _ = ui_tx.send(UiEvent::ApprovalResolved(id));
        }
        ServerMsg::Status(status) => {
            let _ = ui_tx.send(UiEvent::Lifecycle(status));
        }
        ServerMsg::Ended { reason } => {
            let _ = ui_tx.send(UiEvent::Connection(ConnectionState::Ended {
                reason: reason.clone(),
            }));
            if !reason.is_empty() {
                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(reason)));
            }
            return false;
        }
    }
    true
}

fn title_for(info: &SessionInfo) -> String {
    let cwd = info.root.display();
    match &info.branch {
        Some(b) => format!("{cwd}  ⎇ {b}"),
        None => cwd.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::daemonproto::SessionStatus;
    use tokio::net::UnixListener;

    /// The TUI's "end" drops `task_tx`; the bridge must then send `ClientMsg::End`
    /// to the worker. (Isolates the client half of the "press e, session stays
    /// Running" bug — no worker/daemon involved, just the bridge.)
    #[tokio::test]
    async fn dropping_task_tx_sends_end_to_worker() {
        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake worker: read the ClientMsgs the bridge sends until we see End.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if matches!(
                    serde_json::from_str::<ClientMsg>(line.trim()),
                    Ok(ClientMsg::End)
                ) {
                    return true;
                }
            }
            false
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, false));

        // Simulate the pause-menu "end": drop the only task sender.
        drop(task_tx);

        let saw_end = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("timed out waiting for the bridge to send End")
            .unwrap();
        assert!(
            saw_end,
            "dropping task_tx must make the bridge send ClientMsg::End"
        );
        bridge_h.abort();
    }

    /// The reported sequence, which the test above misses: interrupt a turn (`k`),
    /// *then* end (`e`). This is the shape of the "ended the session and the worker
    /// is still running" bug — a plain end works, an end after an interrupt did not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_task_tx_after_an_interrupt_still_sends_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Counters rather than a return value: a fake worker that *returns* closes its
        // socket, which makes the bridge tear itself down — a feedback loop that races
        // whatever is still being written. Observing from the outside keeps the
        // connection open for as long as the test needs it.
        let interrupts = std::sync::Arc::new(AtomicUsize::new(0));
        let ends = std::sync::Arc::new(AtomicUsize::new(0));
        let (si, se) = (interrupts.clone(), ends.clone());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ClientMsg>(line.trim()) {
                    Ok(ClientMsg::Interrupt { .. }) => {
                        si.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(ClientMsg::End) => {
                        se.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel.clone(), false));

        // "k": cancel the in-flight turn. The bridge's watcher turns this into an
        // Interrupt and re-arms a fresh token for the next turn.
        turn_cancel.lock().unwrap().clone().unwrap().cancel();
        await_count(&interrupts, 1, "the interrupt was never forwarded").await;

        // "e": exactly what the pause menu does — send End, drop the sender, then
        // cancel whatever token is current.
        task_tx.send(AgentCmd::End).unwrap();
        drop(task_tx);
        if let Some(tok) = turn_cancel.lock().unwrap().as_ref() {
            tok.cancel();
        }
        await_count(&ends, 1, "ending after an interrupt must still send End").await;

        bridge_h.abort();
        server.abort();
    }

    /// Wait for `counter` to reach `want`, or fail with `why`.
    async fn await_count(counter: &std::sync::atomic::AtomicUsize, want: usize, why: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while counter.load(std::sync::atomic::Ordering::SeqCst) < want {
            assert!(tokio::time::Instant::now() < deadline, "{why}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The explicit end: `e` sends `AgentCmd::End`, and the bridge must forward it as
    /// `ClientMsg::End` exactly once — not twice (the sender is dropped straight
    /// after, which is the backstop path).
    /// On a **current_thread** runtime, deliberately: that is what `attach_socket_ro`
    /// builds, so the writer task and the bridge's own teardown share one thread and
    /// their ordering is decided by the runtime's queue rather than by two CPUs racing.
    /// This is the shape the product actually uses, so it is the shape to assert on.
    #[tokio::test]
    async fn an_explicit_end_is_forwarded_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Counted from outside rather than returned: a fake worker that returns closes
        // its socket, and the bridge deliberately no longer tears down until the worker
        // says `Ended` — so waiting for EOF here would wait forever.
        let ends = std::sync::Arc::new(AtomicUsize::new(0));
        let se = ends.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if matches!(
                    serde_json::from_str::<ClientMsg>(line.trim()),
                    Ok(ClientMsg::End)
                ) {
                    se.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, false));

        // Exactly what the pause menu's "e" does now.
        task_tx.send(AgentCmd::End).unwrap();
        drop(task_tx);

        await_count(&ends, 1, "the explicit end was never forwarded").await;
        // Then settle, so a second End from the hangup path would be caught.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            ends.load(Ordering::SeqCst),
            1,
            "End must be sent once, not duplicated by the hangup"
        );
        bridge_h.abort();
        server.abort();
    }

    /// A read-only client must not be able to end the session it is watching, however
    /// the end was requested.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_only_client_never_sends_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = assert_fs::TempDir::new().unwrap();
        let sock = dir.path().join("w.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let hellos = std::sync::Arc::new(AtomicUsize::new(0));
        let ends = std::sync::Arc::new(AtomicUsize::new(0));
        let (sh, se) = (hellos.clone(), ends.clone());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ClientMsg>(line.trim()) {
                    Ok(ClientMsg::Hello { .. }) => {
                        sh.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(ClientMsg::End) => {
                        se.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, _ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let turn_cancel: TurnCancel =
            std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, true));

        // The Hello proves the bridge is really talking, so "no End" is a decision and
        // not just a connection that never got going.
        await_count(&hellos, 1, "the read-only client never announced itself").await;
        task_tx.send(AgentCmd::End).unwrap();
        drop(task_tx);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            ends.load(Ordering::SeqCst),
            0,
            "a watcher must not end the session it is only watching"
        );
        bridge_h.abort();
        server.abort();
    }

    fn info() -> SessionInfo {
        SessionInfo {
            id: "t".into(),
            root: "/tmp/app".into(),
            task: None,
            status: SessionStatus::Running,
            pid: None,
            branch: Some("main".into()),
            session_name: None,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_translates_both_directions() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake worker: handshake, push events, expect a Message, do an Ask.
        let worker = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains("hello"));

            for m in [
                ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 0,
                    pending_prompts: Vec::new(),
                },
                ServerMsg::Event {
                    seq: 0,
                    event: UiEventMsg::Delta("hi".into()),
                },
            ] {
                w.write_all(encode_line(&m).as_bytes()).await.unwrap();
            }
            w.flush().await.unwrap();

            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(
                line.contains("\"message\"") && line.contains("go"),
                "got {line}"
            );

            w.write_all(
                encode_line(&ServerMsg::Ask {
                    id: 7,
                    question: "ok?".into(),
                    options: Vec::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(
                line.contains("ask_reply") && line.contains("yes") && line.contains('7'),
                "got {line}"
            );
            w.write_all(
                encode_line(&ServerMsg::Ended {
                    reason: String::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
        });

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
        let cancel: TurnCancel = std::sync::Arc::new(std::sync::Mutex::new(None));
        let bridge = tokio::spawn(bridge(stream, ui_tx, task_rx, cancel, false));

        let title = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(title, UiEvent::Wire(UiEventMsg::Title(t)) if t.contains("main")));
        let delta = loop {
            let event = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(event, UiEvent::Wire(UiEventMsg::Delta(ref t)) if t == "hi") {
                break event;
            }
        };
        assert!(matches!(delta, UiEvent::Wire(UiEventMsg::Delta(t)) if t == "hi"));

        task_tx.send(AgentCmd::Message("go".into())).unwrap();

        let ask = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match ask {
            UiEvent::Ask(_id, q, _options, reply) => {
                assert_eq!(q, "ok?");
                reply.send("yes".into()).unwrap();
            }
            other => panic!("expected Ask, got {other:?}"),
        }

        worker.await.unwrap();
        let _ = bridge.await;
    }

    #[test]
    fn reconnect_backoff_starts_at_250ms_and_caps_at_2s() {
        assert_eq!(reconnect_backoff(1), Duration::from_millis(250));
        assert_eq!(reconnect_backoff(2), Duration::from_millis(500));
        assert_eq!(reconnect_backoff(3), Duration::from_secs(1));
        assert_eq!(reconnect_backoff(4), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(20), Duration::from_secs(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unavailable_is_nonterminal_after_the_reconnect_window() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("missing.sock");
        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (task_tx, task_rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let client_sock = sock.clone();
        let supervisor = tokio::spawn(async move {
            supervise_socket_with_window(
                &client_sock,
                ui_tx,
                task_rx,
                cancel,
                false,
                Duration::from_millis(10),
            )
            .await;
        });

        let states = tokio::task::spawn_blocking(move || {
            let mut states = Vec::new();
            while !states
                .iter()
                .any(|event| matches!(event, UiEvent::Connection(ConnectionState::Unavailable)))
            {
                states.push(ui_rx.recv_timeout(Duration::from_secs(3)).unwrap());
            }
            states
        })
        .await
        .unwrap();
        assert!(matches!(
            states.first(),
            Some(UiEvent::Connection(ConnectionState::Connecting))
        ));
        assert!(states.iter().any(|event| matches!(
            event,
            UiEvent::Connection(ConnectionState::Reconnecting { attempt: 1 })
        )));
        assert!(!states.iter().any(|event| matches!(event, UiEvent::Done)));
        task_tx.send(AgentCmd::Detach).unwrap();
        tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_reply_is_resent_only_while_snapshots_list_its_id() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("prompt-reconnect.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server_sock = sock.clone();
        let server = tokio::spawn(async move {
            let prompt = PendingPrompt::Ask {
                id: 7,
                question: "continue?".into(),
                options: Vec::new(),
            };

            let (first, _) = listener.accept().await.unwrap();
            drop(listener);
            let (r, mut w) = first.into_split();
            let mut lines = BufReader::new(r).lines();
            assert!(matches!(
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()),
                Ok(ClientMsg::Hello { .. })
            ));
            w.write_all(
                encode_line(&ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 0,
                    pending_prompts: vec![prompt.clone()],
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            let first_reply: ClientMsg =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                first_reply,
                ClientMsg::AskReply { id: 7, ref answer } if answer == "yes"
            ));
            drop(w);
            let _ = std::fs::remove_file(&server_sock);

            let listener = UnixListener::bind(&server_sock).unwrap();
            let (second, _) = listener.accept().await.unwrap();
            drop(listener);
            let (r, mut w) = second.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello: ClientMsg =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                hello,
                ClientMsg::Hello {
                    since_seq: Some(0),
                    ..
                }
            ));
            w.write_all(
                encode_line(&ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 0,
                    pending_prompts: vec![prompt],
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            let resent: ClientMsg =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                resent,
                ClientMsg::AskReply { id: 7, ref answer } if answer == "yes"
            ));
            drop(w);
            let _ = std::fs::remove_file(&server_sock);

            let listener = UnixListener::bind(&server_sock).unwrap();
            let (third, _) = listener.accept().await.unwrap();
            let (r, mut w) = third.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello: ClientMsg =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                hello,
                ClientMsg::Hello {
                    since_seq: Some(0),
                    ..
                }
            ));
            w.write_all(
                encode_line(&ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 0,
                    pending_prompts: Vec::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(200), lines.next_line())
                    .await
                    .is_err(),
                "a reply whose ID disappeared from Snapshot must not be resent"
            );
            w.write_all(
                encode_line(&ServerMsg::Ended {
                    reason: "complete".into(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
        });

        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (_task_tx, task_rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let client_sock = sock.clone();
        let supervisor = tokio::spawn(async move {
            supervise_socket(&client_sock, ui_tx, task_rx, cancel, false).await;
        });
        let _ui_rx = tokio::task::spawn_blocking(move || loop {
            if let UiEvent::ReplacePrompts(prompts) =
                ui_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                if let Some(UiPrompt::Ask { reply, .. }) = prompts.into_iter().next() {
                    reply.send("yes".into()).unwrap();
                    break ui_rx;
                }
            }
        })
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), supervisor)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_reconnects_with_the_next_contiguous_sequence() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("reconnect.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server_sock = sock.clone();
        let server = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            drop(listener);
            let (r, mut w) = first.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello: ClientMsg =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                hello,
                ClientMsg::Hello {
                    since_seq: None,
                    ..
                }
            ));
            w.write_all(
                encode_line(&ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 1,
                    pending_prompts: Vec::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.write_all(
                encode_line(&ServerMsg::Event {
                    seq: 0,
                    event: UiEventMsg::Notice("first".into()),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            drop(w);
            let _ = std::fs::remove_file(&server_sock);

            let listener = UnixListener::bind(&server_sock).unwrap();
            let (second, _) = listener.accept().await.unwrap();
            let (r, mut w) = second.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello: ClientMsg =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                hello,
                ClientMsg::Hello {
                    since_seq: Some(1),
                    ..
                }
            ));
            w.write_all(
                encode_line(&ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 2,
                    pending_prompts: Vec::new(),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.write_all(
                encode_line(&ServerMsg::Event {
                    seq: 1,
                    event: UiEventMsg::Notice("second".into()),
                })
                .as_bytes(),
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            while let Some(line) = lines.next_line().await.unwrap() {
                if matches!(serde_json::from_str(&line), Ok(ClientMsg::End)) {
                    break;
                }
            }
        });

        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (task_tx, task_rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let client_sock = sock.clone();
        let supervisor = tokio::spawn(async move {
            supervise_socket(&client_sock, ui_tx, task_rx, cancel, false).await;
        });
        let seen = tokio::task::spawn_blocking(move || {
            let mut notices = Vec::new();
            while notices.len() < 2 {
                if let UiEvent::Wire(UiEventMsg::Notice(text)) =
                    ui_rx.recv_timeout(Duration::from_secs(10)).unwrap()
                {
                    notices.push(text);
                }
            }
            notices
        })
        .await
        .unwrap();
        assert_eq!(seen, vec!["first", "second"]);
        task_tx.send(AgentCmd::End).unwrap();
        tokio::time::timeout(Duration::from_secs(3), supervisor)
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn malformed_terminal_journal_is_an_error() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let journal = tmp.path().join("events.jsonl");
        let lines = [
            serde_json::to_string(&UiEventMsg::ToolUse("read foo".into())).unwrap(),
            "{ not json".to_string(),
            serde_json::to_string(&UiEventMsg::Final("done".into())).unwrap(),
        ];
        std::fs::write(&journal, format!("{}\n", lines.join("\n"))).unwrap();

        let error = crate::agent::socket_ui::read_journal(&journal).unwrap_err();
        assert!(error.to_string().contains("sequence 1"), "{error:#}");
    }

    #[test]
    fn missing_terminal_journal_is_an_error() {
        let error =
            crate::agent::socket_ui::read_journal(std::path::Path::new("/nope/missing.jsonl"))
                .unwrap_err();
        assert!(error.to_string().contains("opening journal"), "{error:#}");
    }
}
