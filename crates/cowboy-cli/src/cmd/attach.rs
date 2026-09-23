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
        let done_tx = ui_tx.clone();
        rt.block_on(async move {
            supervise_socket(&sock, ui_tx, task_rx, bridge_cancel, read_only).await;
        });
        // Whatever made the bridge stop, the TUI must hear that it has. The event
        // loop holds a sender of its own, so the channel never closes to tell it —
        // and "ending session…" used to wait forever on an `Ended` the bridge had
        // stopped reading. A duplicate `Done` after a real one is harmless; after a
        // detach the loop has already exited and this send just fails.
        let _ = done_tx.send(UiEvent::Done);
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
    supervise(
        Supervision::production(sock, read_only),
        None,
        ui_tx,
        task_rx,
        turn_cancel,
    )
    .await;
}

/// Where a session went while this client could not reach it.
#[derive(Debug)]
enum Resolution {
    /// Still live, at this worker socket (possibly a new one).
    Live(std::path::PathBuf),
    /// Over: its journal is on disk (the initial attach's replay fallback).
    Ended {
        journal: std::path::PathBuf,
        status: String,
    },
    /// No authoritative answer (daemon unreachable, unknown id): keep retrying.
    Unknown,
}

/// Ask where a session is, by id. Injected so tests needn't reach a real daemon.
type Resolver = Box<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Resolution> + Send>>
        + Send
        + Sync,
>;

/// The daemon's view, mirroring the initial attach's probe-then-replay.
fn daemon_resolver() -> Resolver {
    Box::new(|id| {
        Box::pin(async move {
            match crate::cmd::daemon::request(DaemonReq::AttachSession { id }).await {
                Ok(DaemonResp::Attach {
                    target: AttachTarget::Live { worker_sock },
                }) => Resolution::Live(worker_sock),
                Ok(DaemonResp::Attach {
                    target:
                        AttachTarget::Replay {
                            journal_path,
                            status,
                        },
                }) => Resolution::Ended {
                    journal: journal_path,
                    status: format!("{status:?}"),
                },
                _ => Resolution::Unknown,
            }
        })
    })
}

/// How a supervised attachment behaves; the knobs exist for tests.
struct Supervision {
    sock: std::path::PathBuf,
    read_only: bool,
    /// How long to retry quickly (showing `Reconnecting`) before `Unavailable`.
    reconnect_window: Duration,
    /// The retry interval once `Unavailable` — slower, but never giving up.
    unavailable_retry: Duration,
    resolve: Resolver,
}

impl Supervision {
    fn production(sock: &std::path::Path, read_only: bool) -> Self {
        Self {
            sock: sock.to_path_buf(),
            read_only,
            reconnect_window: Duration::from_secs(30),
            unavailable_retry: Duration::from_secs(5),
            resolve: daemon_resolver(),
        }
    }
}

/// One line from the worker, classified. Shared by every reader so a test cannot
/// exercise different parsing from the one the product uses.
#[derive(Debug)]
enum Frame {
    Msg(Box<ServerMsg>),
    /// A well-formed journal event this build has no variant for — a newer worker.
    /// Its sequence number is still authoritative, so the client skips it and stays
    /// contiguous instead of reconnecting forever over something it can never parse.
    UnknownEvent {
        seq: u64,
    },
    /// Anything else unparseable; skipped.
    Unparsed,
}

fn parse_frame(line: &str) -> Frame {
    if let Ok(msg) = serde_json::from_str::<ServerMsg>(line) {
        return Frame::Msg(Box::new(msg));
    }
    let seq = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("event")?.get("seq")?.as_u64());
    match seq {
        Some(seq) => Frame::UnknownEvent { seq },
        None => Frame::Unparsed,
    }
}

/// Reconnect bookkeeping across attempts.
#[derive(Default)]
struct Backoff {
    started: Option<tokio::time::Instant>,
    attempt: u32,
    unavailable: bool,
}

impl Backoff {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What to do after a failed or lost connection.
enum Next {
    Retry,
    Stop,
    /// The daemon says the session is over; finish from its journal.
    Ended {
        journal: std::path::PathBuf,
        status: String,
    },
}

async fn supervise(
    mut cfg: Supervision,
    mut first: Option<UnixStream>,
    ui_tx: Sender<UiEvent>,
    mut task_rx: Receiver<AgentCmd>,
    turn_cancel: TurnCancel,
) {
    let read_only = cfg.read_only;
    let (reply_tx, mut reply_rx) = unbounded_channel::<ClientMsg>();
    let mut next_seq: Option<u64> = None;
    let mut authoritative_prompts = HashSet::new();
    let mut pending_replies = HashMap::<u64, ClientMsg>::new();
    let mut backoff = Backoff::default();
    // Learned from the first Snapshot; what the daemon is asked about later.
    let mut session_id: Option<String> = None;
    let mut warned_unknown = false;

    if ui_tx
        .send(UiEvent::Connection(ConnectionState::Connecting))
        .is_err()
    {
        return;
    }

    // Bundled so each failure site is one line rather than eight arguments.
    macro_rules! lost {
        () => {
            match reconnect_or_stop(
                &mut cfg,
                session_id.as_deref(),
                &ui_tx,
                &mut task_rx,
                &mut reply_rx,
                &authoritative_prompts,
                &mut pending_replies,
                &mut backoff,
            )
            .await
            {
                Next::Retry => continue,
                Next::Stop => return,
                Next::Ended { journal, status } => {
                    finish_from_journal(&ui_tx, &journal, &status, next_seq.unwrap_or(0));
                    return;
                }
            }
        };
    }

    loop {
        let stream = match first.take() {
            Some(stream) => stream,
            None => match crate::localsock::connect(&cfg.sock).await {
                Ok(stream) => stream,
                Err(_) => lost!(),
            },
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
            lost!();
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
                    let frame = parse_frame(line.trim());
                    line.clear();
                    let msg = match frame {
                        Frame::Msg(msg) => *msg,
                        Frame::UnknownEvent { seq } => {
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
                            if !std::mem::replace(&mut warned_unknown, true) {
                                let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(
                                    "skipped an event this client doesn't understand — upgrade cowboy"
                                        .into(),
                                )));
                            }
                            expected += 1;
                            next_seq = Some(expected);
                            if !synced && expected == boundary {
                                if ui_tx.send(UiEvent::Connection(ConnectionState::Live)).is_err() {
                                    return;
                                }
                                synced = true;
                                backoff.reset();
                            }
                            continue;
                        }
                        Frame::Unparsed => {
                            tracing::debug!("skipping a worker frame this client cannot parse");
                            continue;
                        }
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
                            session_id = Some(info.id.clone());
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
                                backoff.reset();
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
                                backoff.reset();
                            }
                        }
                        ServerMsg::Ask { id, question, options } => {
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
                        // Addressed to this client alone and never journaled, so it
                        // carries no sequence number.
                        ServerMsg::CommandReply { text } => {
                            if ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(text))).is_err() {
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
                                if wrote && matches!(msg, ClientMsg::End) {
                                    await_ended(&mut reader, &ui_tx).await;
                                }
                                return;
                            }
                            if !wrote {
                                reconnect = true;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            let msg = if read_only { ClientMsg::Detach } else { ClientMsg::End };
                            if write_client(&mut w, &msg).await && !read_only {
                                await_ended(&mut reader, &ui_tx).await;
                            }
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

        lost!();
    }
}

/// How long "ending session…" waits for the worker to confirm before the TUI is
/// told the session is over anyway. Ending runs teardown (the sandbox, finalizing
/// the journal), so it isn't instant; but the session is ending either way, and a
/// client must never outwait a worker that is gone.
const END_CONFIRM_WAIT: Duration = Duration::from_secs(15);

/// After sending `End`: read until the worker's `Ended` (or its socket closes, or
/// [`END_CONFIRM_WAIT`] passes), then tell the TUI the session is done. Events that
/// arrive meanwhile (the last notices of teardown) are still shown.
async fn await_ended(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    ui_tx: &Sender<UiEvent>,
) {
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + END_CONFIRM_WAIT;
    loop {
        line.clear();
        match tokio::time::timeout_at(deadline, reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => match serde_json::from_str::<ServerMsg>(line.trim()) {
                Ok(ServerMsg::Ended { reason }) => {
                    let _ = ui_tx.send(UiEvent::Connection(ConnectionState::Ended {
                        reason: reason.clone(),
                    }));
                    if !reason.is_empty() {
                        let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(reason)));
                    }
                    break;
                }
                Ok(ServerMsg::Event { event, .. }) => {
                    let _ = ui_tx.send(UiEvent::Wire(event));
                }
                _ => {}
            },
            // EOF, a read error, or the wait ran out: the session is over either way.
            _ => break,
        }
    }
    let _ = ui_tx.send(UiEvent::Done);
}

/// After a lost connection: wait out the backoff (answering the UI meanwhile), and
/// say whether to retry. Inside the reconnect window this retries fast. Past it the
/// client is `Unavailable` but never gives up: it retries at a capped interval and
/// asks the daemon where the session went, so a worker that restarted is rejoined
/// and one that ended is finished from its journal.
#[allow(clippy::too_many_arguments)]
async fn reconnect_or_stop(
    cfg: &mut Supervision,
    session_id: Option<&str>,
    ui_tx: &Sender<UiEvent>,
    task_rx: &mut Receiver<AgentCmd>,
    reply_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientMsg>,
    authoritative_prompts: &HashSet<u64>,
    pending_replies: &mut HashMap<u64, ClientMsg>,
    backoff: &mut Backoff,
) -> Next {
    let started = *backoff
        .started
        .get_or_insert_with(tokio::time::Instant::now);
    backoff.attempt = backoff.attempt.saturating_add(1);
    let delay = if backoff.unavailable || started.elapsed() >= cfg.reconnect_window {
        if !std::mem::replace(&mut backoff.unavailable, true)
            && ui_tx
                .send(UiEvent::Connection(ConnectionState::Unavailable))
                .is_err()
        {
            return Next::Stop;
        }
        cfg.unavailable_retry
    } else {
        if ui_tx
            .send(UiEvent::Connection(ConnectionState::Reconnecting {
                attempt: backoff.attempt,
            }))
            .is_err()
        {
            return Next::Stop;
        }
        reconnect_backoff(backoff.attempt)
    };
    let notice = if backoff.unavailable {
        "not connected to the session (still retrying) — the command was not sent"
    } else {
        "command was not sent while the session was reconnecting"
    };
    if wait_disconnected(
        delay,
        notice,
        ui_tx,
        task_rx,
        reply_rx,
        authoritative_prompts,
        pending_replies,
    )
    .await
    {
        return Next::Stop;
    }
    if backoff.unavailable {
        if let Some(id) = session_id {
            match (cfg.resolve)(id.to_string()).await {
                Resolution::Live(sock) => cfg.sock = sock,
                Resolution::Ended { journal, status } => return Next::Ended { journal, status },
                Resolution::Unknown => {}
            }
        }
    }
    Next::Retry
}

/// Sit out `delay` without a connection, keeping prompt replies for resend and
/// telling the user about every command that is dropped. True means stop (the user
/// detached or ended, or the UI went away).
async fn wait_disconnected(
    delay: Duration,
    notice: &str,
    ui_tx: &Sender<UiEvent>,
    task_rx: &mut Receiver<AgentCmd>,
    reply_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientMsg>,
    authoritative_prompts: &HashSet<u64>,
    pending_replies: &mut HashMap<u64, ClientMsg>,
) -> bool {
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
                        let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(notice.into())));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
        }
    }
}

/// The session ended while this client was away: show what it missed from the
/// journal, then end the view the way a live `Ended` would.
fn finish_from_journal(
    ui_tx: &Sender<UiEvent>,
    journal: &std::path::Path,
    status: &str,
    next_seq: u64,
) {
    match crate::agent::socket_ui::read_journal(journal) {
        Ok(events) => {
            for event in events.into_iter().skip(next_seq as usize) {
                if ui_tx.send(UiEvent::Wire(event)).is_err() {
                    return;
                }
            }
        }
        Err(e) => {
            let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(format!(
                "the session's journal could not be read: {e:#}"
            ))));
        }
    }
    let reason = format!("session ended while disconnected ({status})");
    let _ = ui_tx.send(UiEvent::Connection(ConnectionState::Ended {
        reason: reason.clone(),
    }));
    let _ = ui_tx.send(UiEvent::Wire(UiEventMsg::Notice(reason)));
    let _ = ui_tx.send(UiEvent::Done);
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
        AgentCmd::Command(c) => (ClientMsg::Command(c), false),
        AgentCmd::Detach => (ClientMsg::Detach, true),
        AgentCmd::End => (ClientMsg::End, true),
    }
}

/// Bridge an already-connected worker `stream` to the UI channels. Returns when the
/// worker ends or the UI hangs up. A `read_only` client never forwards input, so it
/// can watch without driving the session.
///
/// This is the production supervisor started on a given connection — not a second
/// implementation of the protocol. It used to be one, and it had drifted (it skipped
/// every frame it could not parse while the real client reconnected on them), so its
/// tests were exercising behaviour no user ran. Reconnects go to the socket the
/// stream is connected to.
pub async fn bridge(
    stream: UnixStream,
    ui_tx: Sender<UiEvent>,
    task_rx: Receiver<AgentCmd>,
    turn_cancel: TurnCancel,
    read_only: bool,
) -> Result<()> {
    let sock = stream
        .peer_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(std::path::Path::to_path_buf))
        .unwrap_or_default();
    supervise(
        Supervision::production(&sock, read_only),
        Some(stream),
        ui_tx,
        task_rx,
        turn_cancel,
    )
    .await;
    Ok(())
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

    /// Ending must reach `Done` — the TUI's "ending session…" waits on it, and holds a
    /// sender of its own, so nothing else will ever release it. It used to hang there
    /// forever: the bridge wrote `End` and returned without reading the worker's
    /// `Ended`. Covered both ways a worker can go: it confirms, or it just closes.
    #[tokio::test]
    async fn ending_the_session_always_reaches_done() {
        for confirms in [true, false] {
            let dir = assert_fs::TempDir::new().unwrap();
            let sock = dir.path().join("w.sock");
            let listener = UnixListener::bind(&sock).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                let _hello = lines.next_line().await;
                let snapshot = ServerMsg::Snapshot {
                    info: info(),
                    journal_len: 0,
                    pending_prompts: Vec::new(),
                };
                w.write_all(encode_line(&snapshot).as_bytes())
                    .await
                    .unwrap();
                while let Ok(Some(line)) = lines.next_line().await {
                    if matches!(serde_json::from_str(line.trim()), Ok(ClientMsg::End)) {
                        if confirms {
                            let ended = ServerMsg::Ended {
                                reason: "session ended by user".into(),
                            };
                            let _ = w.write_all(encode_line(&ended).as_bytes()).await;
                        }
                        return; // dropping both halves closes the socket
                    }
                }
            });

            let stream = UnixStream::connect(&sock).await.unwrap();
            let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
            let (task_tx, task_rx) = std::sync::mpsc::channel::<AgentCmd>();
            let turn_cancel: TurnCancel =
                std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
            // Keep a sender alive, as the TUI's event loop does.
            let _loop_tx = ui_tx.clone();
            let bridge_h = tokio::spawn(bridge(stream, ui_tx, task_rx, turn_cancel, false));
            // Let the snapshot land, then end exactly as the double Ctrl-C does.
            tokio::time::sleep(Duration::from_millis(100)).await;
            task_tx.send(AgentCmd::End).unwrap();
            drop(task_tx);

            let done = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if let Ok(UiEvent::Done) = ui_rx.recv_timeout(Duration::from_millis(100)) {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap();
            assert!(done, "ending must reach Done (worker confirms: {confirms})");
            let _ = server.await;
            let _ = tokio::time::timeout(Duration::from_secs(2), bridge_h).await;
        }
    }

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

        // The supervisor reports its connection state first; the Snapshot's title follows.
        let title = loop {
            let event = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if !matches!(event, UiEvent::Connection(_)) {
                break event;
            }
        };
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
            supervise(
                test_cfg(&client_sock, Duration::from_millis(10), |_| {
                    Resolution::Unknown
                }),
                None,
                ui_tx,
                task_rx,
                cancel,
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

    /// A supervision for tests: a short reconnect window, fast unavailable retries,
    /// and a scripted resolver instead of the daemon.
    fn test_cfg(
        sock: &std::path::Path,
        window: Duration,
        resolve: impl Fn(String) -> Resolution + Send + Sync + 'static,
    ) -> Supervision {
        let resolve = std::sync::Arc::new(resolve);
        Supervision {
            sock: sock.to_path_buf(),
            read_only: false,
            reconnect_window: window,
            unavailable_retry: Duration::from_millis(30),
            resolve: Box::new(move |id| {
                let resolve = resolve.clone();
                Box::pin(async move { resolve(id) })
            }),
        }
    }

    async fn send_all(w: &mut tokio::net::unix::OwnedWriteHalf, msgs: &[ServerMsg]) {
        for m in msgs {
            w.write_all(encode_line(m).as_bytes()).await.unwrap();
        }
        w.flush().await.unwrap();
    }

    fn notice(seq: u64, text: &str) -> ServerMsg {
        ServerMsg::Event {
            seq,
            event: UiEventMsg::Notice(text.into()),
        }
    }

    /// Drain UI events until `Done` (or a timeout, which fails the test).
    fn collect_until_done(ui_rx: std::sync::mpsc::Receiver<UiEvent>) -> Vec<UiEvent> {
        let mut seen = Vec::new();
        loop {
            let ev = ui_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("timed out waiting for Done");
            let done = matches!(ev, UiEvent::Done);
            seen.push(ev);
            if done {
                return seen;
            }
        }
    }

    fn notices(events: &[UiEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                UiEvent::Wire(UiEventMsg::Notice(t)) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn frames_are_classified_for_forward_compatibility() {
        assert!(matches!(
            parse_frame(&serde_json::to_string(&notice(4, "x")).unwrap()),
            Frame::Msg(m) if matches!(*m, ServerMsg::Event { seq: 4, .. })
        ));
        assert!(matches!(
            parse_frame(r#"{"event":{"seq":9,"event":{"from_the_future":{"x":1}}}}"#),
            Frame::UnknownEvent { seq: 9 }
        ));
        assert!(matches!(
            parse_frame(r#"{"from_the_future":{"x":1}}"#),
            Frame::Unparsed
        ));
        assert!(matches!(parse_frame("not json"), Frame::Unparsed));
    }

    /// One event variant from a newer worker must not cost the connection: it is
    /// skipped in sequence (with one notice), and so is any other frame this build
    /// cannot read. Previously either made the client reconnect, receive the same
    /// frame again, and eventually give up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_event_is_skipped_without_reconnecting() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("future.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(listener);
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            lines.next_line().await.unwrap().unwrap(); // hello
            send_all(
                &mut w,
                &[
                    ServerMsg::Snapshot {
                        info: info(),
                        journal_len: 4,
                        pending_prompts: Vec::new(),
                    },
                    notice(0, "a"),
                ],
            )
            .await;
            w.write_all(
                concat!(
                    r#"{"event":{"seq":1,"event":{"from_the_future":{"x":1}}}}"#,
                    "\n",
                    "garbage\n",
                    r#"{"brand_new_server_msg":{"y":2}}"#,
                    "\n",
                    r#"{"event":{"seq":2,"event":"another_new_one"}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            send_all(
                &mut w,
                &[
                    notice(3, "b"),
                    ServerMsg::Ended {
                        reason: String::new(),
                    },
                ],
            )
            .await;
        });

        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (_task_tx, task_rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let client_sock = sock.clone();
        let supervisor = tokio::spawn(async move {
            supervise(
                test_cfg(&client_sock, Duration::from_secs(5), |_| {
                    Resolution::Unknown
                }),
                None,
                ui_tx,
                task_rx,
                cancel,
            )
            .await;
        });
        let events = tokio::task::spawn_blocking(move || collect_until_done(ui_rx))
            .await
            .unwrap();
        assert_eq!(
            notices(&events),
            vec![
                "a".to_string(),
                "skipped an event this client doesn't understand — upgrade cowboy".into(),
                "b".into(),
            ]
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, UiEvent::Connection(ConnectionState::Live))));
        assert!(!events
            .iter()
            .any(|e| matches!(e, UiEvent::Connection(ConnectionState::Reconnecting { .. }))));
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), supervisor)
            .await
            .unwrap()
            .unwrap();
    }

    /// Past the reconnect window the client is `Unavailable`, but it keeps trying:
    /// it asks the daemon where the session is and rejoins it there, resuming at the
    /// next sequence. Commands typed meanwhile are refused out loud.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unavailable_keeps_retrying_and_rejoins_where_the_daemon_says() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = assert_fs::TempDir::new().unwrap();
        let old_sock = tmp.path().join("old.sock");
        let new_sock = tmp.path().join("new.sock");
        let old = UnixListener::bind(&old_sock).unwrap();
        let new = UnixListener::bind(&new_sock).unwrap();
        let server_old = old_sock.clone();
        let server = tokio::spawn(async move {
            let (first, _) = old.accept().await.unwrap();
            drop(old);
            let (r, mut w) = first.into_split();
            let mut lines = BufReader::new(r).lines();
            lines.next_line().await.unwrap().unwrap();
            send_all(
                &mut w,
                &[
                    ServerMsg::Snapshot {
                        info: info(),
                        journal_len: 1,
                        pending_prompts: Vec::new(),
                    },
                    notice(0, "first"),
                ],
            )
            .await;
            drop(w);
            drop(lines);
            let _ = std::fs::remove_file(&server_old);

            let (second, _) = new.accept().await.unwrap();
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
            send_all(
                &mut w,
                &[
                    ServerMsg::Snapshot {
                        info: info(),
                        journal_len: 2,
                        pending_prompts: Vec::new(),
                    },
                    notice(1, "second"),
                ],
            )
            .await;
            while let Some(line) = lines.next_line().await.unwrap() {
                if matches!(serde_json::from_str(&line), Ok(ClientMsg::End)) {
                    break;
                }
            }
        });

        // "Don't know" a few times first, so the client demonstrably keeps going.
        let asked = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let resolved = new_sock.clone();
        let cfg = test_cfg(&old_sock, Duration::from_millis(50), move |id| {
            assert_eq!(id, "t");
            if counter.fetch_add(1, Ordering::SeqCst) < 3 {
                Resolution::Unknown
            } else {
                Resolution::Live(resolved.clone())
            }
        });
        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (task_tx, task_rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let supervisor = tokio::spawn(async move {
            supervise(cfg, None, ui_tx, task_rx, cancel).await;
        });
        let seen = tokio::task::spawn_blocking(move || {
            let mut seen = Vec::new();
            let mut sent = false;
            loop {
                let ev = ui_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if !sent && matches!(ev, UiEvent::Connection(ConnectionState::Unavailable)) {
                    task_tx.send(AgentCmd::Message("hello?".into())).unwrap();
                    sent = true;
                }
                let done = matches!(&ev, UiEvent::Wire(UiEventMsg::Notice(t)) if t == "second");
                seen.push(ev);
                if done {
                    // Wait for the resumed connection to report itself live, then end.
                    loop {
                        let ev = ui_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        let live = matches!(ev, UiEvent::Connection(ConnectionState::Live));
                        seen.push(ev);
                        if live {
                            break;
                        }
                    }
                    task_tx.send(AgentCmd::End).unwrap();
                    return seen;
                }
            }
        })
        .await
        .unwrap();
        let texts = notices(&seen);
        assert_eq!(texts.first().map(String::as_str), Some("first"));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("not connected to the session")),
            "a command dropped while unavailable must be reported: {texts:?}"
        );
        assert!(asked.load(Ordering::SeqCst) >= 4);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), supervisor)
            .await
            .unwrap()
            .unwrap();
    }

    /// A session that ended while the client was away is finished from its journal
    /// (the events it missed, then an end), rather than left `Unavailable` forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_that_ended_meanwhile_is_finished_from_its_journal() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sock = tmp.path().join("gone.sock");
        let journal = tmp.path().join("events.jsonl");
        let lines: Vec<String> = ["first", "second", "third"]
            .iter()
            .map(|t| serde_json::to_string(&UiEventMsg::Notice((*t).into())).unwrap())
            .collect();
        std::fs::write(&journal, format!("{}\n", lines.join("\n"))).unwrap();

        let listener = UnixListener::bind(&sock).unwrap();
        let server_sock = sock.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(listener);
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            lines.next_line().await.unwrap().unwrap();
            send_all(
                &mut w,
                &[
                    ServerMsg::Snapshot {
                        info: info(),
                        journal_len: 1,
                        pending_prompts: Vec::new(),
                    },
                    notice(0, "first"),
                ],
            )
            .await;
            let _ = std::fs::remove_file(&server_sock);
        });

        let resolved = journal.clone();
        let cfg = test_cfg(&sock, Duration::from_millis(50), move |_| {
            Resolution::Ended {
                journal: resolved.clone(),
                status: "Completed".into(),
            }
        });
        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (_task_tx, task_rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::Mutex::new(Some(CancellationToken::new())));
        let supervisor = tokio::spawn(async move {
            supervise(cfg, None, ui_tx, task_rx, cancel).await;
        });
        let events = tokio::task::spawn_blocking(move || collect_until_done(ui_rx))
            .await
            .unwrap();
        let texts = notices(&events);
        assert_eq!(&texts[..3], ["first", "second", "third"]);
        assert!(events
            .iter()
            .any(|e| matches!(e, UiEvent::Connection(ConnectionState::Ended { .. }))));
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), supervisor)
            .await
            .unwrap()
            .unwrap();
    }
}
